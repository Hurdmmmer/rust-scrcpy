use std::ffi::c_void;

use windows::Win32::Foundation::BOOL;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_QUERY_DESC,
    D3D11_QUERY_EVENT, D3D11_RESOURCE_MISC_SHARED, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    ID3D11Asynchronous, ID3D11Query, ID3D11Resource, ID3D11Texture2D,
};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIResource;
use windows::core::Interface;

use crate::scrcpy::decode_core::gpu_direct_output::context::D3D11Context;

/// D3D11 纹理槽：封装一张共享纹理及其元数据。
struct TextureSlot {
    /// D3D11 纹理对象（由本 Device 创建，CPU 通过 UpdateSubresource 写入）
    texture: ID3D11Texture2D,
    /// DXGI legacy 共享句柄（Windows HANDLE 值），Flutter/ANGLE 通过此句柄
    /// 在自己的 D3D11 Device 上打开同一张纹理进行读取/渲染。
    shared_handle: HANDLE,
    /// 纹理的宽度（像素）
    width: u32,
    /// 纹理的高度（像素）
    height: u32,
}

/// 生产链路 D3D11 纹理上传器：CPU BGRA 数据 → DXGI 共享纹理句柄
///
/// 职责：
///   1. 管理双缓冲纹理槽（两张 D3D11 纹理交替写入，避免写读冲突）；
///   2. 将 CPU 侧 BGRA 像素数据通过 UpdateSubresource 上传到 GPU 纹理；
///   3. ★ 使用 D3D11 Event Query 等待 GPU 真正写入完成，再返回共享句柄；
///   4. 返回 DXGI legacy 共享句柄（u64），供 Flutter/ANGLE 跨 Device 读取。
///
/// 【DXGI Legacy Shared Handle 说明】
///   使用 D3D11_RESOURCE_MISC_SHARED（legacy 模式）而不是
///   D3D11_RESOURCE_MISC_SHARED_NTHANDLE（NT handle 模式），是因为
///   Flutter Windows 外部纹理系统（ANGLE EGL_D3D_TEXTURE_2D_SHARE_HANDLE_ANGLE
///   路径）对 legacy handle 的兼容性更稳定。
pub struct D3D11TextureUploader {
    /// D3D11 设备上下文（用于创建纹理、执行命令）
    ctx: D3D11Context,
    /// 双缓冲槽：索引 0 和 1 交替使用。
    /// Some 表示已分配，None 表示尚未分配或尺寸变化后被清空。
    slots: Vec<Option<TextureSlot>>,
    /// 帧计数器，每帧 +1，模 2 决定写哪个槽。
    frame_index: u64,
    /// ★ GPU 同步查询对象（D3D11_QUERY_EVENT 类型）。
    ///
    /// 在每帧 Flush() 之后，用 End() + GetData() 轮询，等待 GPU 真正完成
    /// UpdateSubresource 的写入，然后才把共享句柄交给 Flutter。
    ///
    /// 该对象在 new_with_context() 时一次性创建，之后每帧复用，避免重复分配。
    sync_query: ID3D11Query,
}

impl D3D11TextureUploader {
    /// 创建上传器。
    ///
    /// 同时会预先创建好 GPU 同步查询对象（D3D11_QUERY_EVENT），后续每帧复用。
    pub fn new_with_context(ctx: &D3D11Context) -> Result<Self, String> {
        // 创建 D3D11_QUERY_EVENT 查询对象，整个生命周期只创建一次，每帧复用。
        let sync_query = Self::create_event_query(ctx)?;

        Ok(Self {
            ctx: ctx.clone(),
            // 双缓冲：预留 2 个槽，初始都为 None（懒分配，首帧才创建纹理）。
            slots: vec![None, None],
            frame_index: 0,
            sync_query,
        })
    }

    /// 将一帧 BGRA 像素数据上传到 GPU 共享纹理，返回共享句柄（u64）。
    ///
    /// # 参数
    /// - `width` / `height`：帧分辨率（像素）。旋转或分辨率变化时值会改变，
    ///   此时该槽的旧纹理会被销毁并重建为新尺寸。
    /// - `_pts`：解码时间戳（当前链路未使用，保留以备后续同步用）。
    /// - `data`：CPU 内存中的 BGRA 像素数据，大小必须 >= width * height * 4。
    ///
    /// # 返回
    /// 成功时返回 DXGI 共享句柄（u64），Flutter C++ 层通过此句柄打开纹理渲染。
    pub fn upload_bgra_frame(
        &mut self,
        width: u32,
        height: u32,
        _pts: i64,
        data: &[u8],
    ) -> Result<u64, String> {
        // ── 入参校验 ──────────────────────────────────────────────────────────
        if width == 0 || height == 0 {
            return Err("invalid frame size: width or height is zero".to_string());
        }
        // 计算期望的字节数，使用 checked_mul 避免溢出
        let expect = width
            .checked_mul(height)
            .and_then(|v| v.checked_mul(4))
            .ok_or_else(|| "frame size overflow (width * height * 4 > u32::MAX)".to_string())?
            as usize;
        if data.len() < expect {
            return Err(format!(
                "BGRA data too short: got={} bytes, expect={} bytes ({}x{}x4)",
                data.len(),
                expect,
                width,
                height
            ));
        }

        // ── 双缓冲槽选择 ──────────────────────────────────────────────────────
        // 帧计数模 2，奇偶帧分别写槽 0 和槽 1，互不干扰。
        // 这样当 Flutter 在读槽 0（frame N）时，Rust 写的是槽 1（frame N+1），
        // 下一帧才会轮回写槽 0（frame N+2），给 Flutter 足够的时间完成渲染。
        let idx = (self.frame_index as usize) % 2;

        // 检查当前槽是否需要重建（首次使用，或旋转/分辨率变化后尺寸不匹配）
        let need_recreate = match &self.slots[idx] {
            Some(slot) => slot.width != width || slot.height != height,
            None => true,
        };
        if need_recreate {
            // 旧纹理（如有）随 Option 被 drop，D3D11 对象通过 windows-rs 的
            // Drop 自动 Release。新纹理按当前分辨率重新分配。
            self.slots[idx] = Some(self.create_slot(width, height)?);
        }

        // ── 取出当前槽的纹理引用 ─────────────────────────────────────────────
        let slot = self
            .slots
            .get(idx)
            .and_then(|s| s.as_ref())
            .ok_or_else(|| "texture slot missing after create".to_string())?;

        // 将 ID3D11Texture2D 向上转型为 ID3D11Resource（UpdateSubresource 所需）
        let resource: ID3D11Resource = slot
            .texture
            .cast()
            .map_err(|e| format!("cast ID3D11Texture2D to ID3D11Resource failed: {}", e))?;

        unsafe {
            // ── 步骤 1：CPU → GPU 数据上传 ────────────────────────────────────
            //
            // UpdateSubresource 把 CPU 内存 data[] 的内容复制到 D3D11 纹理。
            //
            // 参数说明：
            //   - &resource     : 目标纹理（已向上转型）
            //   - 0             : Subresource 索引，MipLevel=0（全尺寸）
            //   - None          : 目标矩形区域，None = 覆盖整张纹理
            //   - data.as_ptr() : 源数据指针（CPU BGRA 像素）
            //   - width * 4     : 源数据每行字节数（BGRA = 4 bytes/pixel，无 padding）
            //   - 0             : 深度切片步长，2D 纹理填 0
            //
            // 注意：此调用在逻辑上是「发出 GPU 命令」，GPU 实际执行是异步的。
            self.ctx.immediate_context().UpdateSubresource(
                &resource,
                0,
                None,
                data.as_ptr() as *const c_void,
                width * 4, // BGRA 每行 = 宽 * 4 字节，数据已由上游去除 stride padding
                0,
            );

            // ── 步骤 2：提交命令队列 ──────────────────────────────────────────
            //
            // Flush() 把当前命令队列里的所有命令（包括上面的 UpdateSubresource）
            // 提交给 GPU 驱动。「提交」≠「执行完成」，GPU 是流水线异步处理的。
            //
            // 跨 Device 共享纹理要求：在另一个 Device（Flutter 的 ANGLE Device）
            // 读取纹理前，产生方 Device 必须 Flush，确保命令已进入 GPU 驱动队列。
            self.ctx.immediate_context().Flush();

            // ── 步骤 3：★ 等待 GPU 真正完成写入（花屏修复核心）────────────────
            //
            // Flush() 之后 GPU 仍在异步执行。我们在这里插入一个 GPU 同步点，
            // 确保 GPU 确实执行完了 UpdateSubresource，像素数据稳定落地在纹理里，
            // 再把共享句柄交给 Flutter 读取。
            //
            // 如果跳过这一步，Flutter/ANGLE 可能读到「半写状态」的纹理，
            // 导致花屏、大块色块、彩条、马赛克等视觉异常。
            self.wait_gpu_write_done();
        }

        // 帧计数 +1（saturating_add 防止 u64 溢出，理论上不会触发）
        self.frame_index = self.frame_index.saturating_add(1);

        // 返回 DXGI 共享句柄（HANDLE 值转为 u64），供 Flutter C++ 层跨 Device 打开
        Ok(slot.shared_handle.0 as usize as u64)
    }

    // ─────────────────────────────────────────────────────────────────────────
    // 私有辅助方法
    // ─────────────────────────────────────────────────────────────────────────

    /// 创建一个新的纹理槽（D3D11 纹理 + DXGI 共享句柄）。
    ///
    /// 在以下情况会调用：
    ///   - 槽首次使用（slots[idx] = None）
    ///   - 分辨率变化（旋转、scrcpy 重配置等）
    fn create_slot(&self, width: u32, height: u32) -> Result<TextureSlot, String> {
        // ── 纹理描述符 ────────────────────────────────────────────────────────
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,   // 不需要 mipmap，视频帧只有全尺寸一级
            ArraySize: 1,   // 单张纹理，不是数组
            // BGRA 格式：与 FFmpeg 输出（AV_PIX_FMT_BGRA）和 Flutter 期望
            // （kFlutterDesktopPixelFormatBGRA8888）严格对齐，避免颜色通道错位。
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,   // 不开 MSAA（视频纹理不需要抗锯齿）
                Quality: 0,
            },
            // DEFAULT：GPU 可读写，CPU 通过 UpdateSubresource 间接写入。
            // DYNAMIC 或 STAGING 无法创建 DXGI 共享纹理。
            Usage: D3D11_USAGE_DEFAULT,
            // SHADER_RESOURCE：ANGLE 将此纹理作为着色器输入（纹理采样器）。
            // RENDER_TARGET：Flutter Windows 外部纹理绑定路径的额外要求，
            //                某些驱动/ANGLE 版本需要此 flag 才能正确访问共享纹理。
            BindFlags: (D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET).0 as u32,
            CPUAccessFlags: 0, // CPU 不直接 Map 此纹理，只用 UpdateSubresource
            // ★ D3D11_RESOURCE_MISC_SHARED（legacy shared handle）
            //
            // 使用 legacy 模式而非 SHARED_NTHANDLE 模式，是因为 Flutter 的 ANGLE
            // 通过 EGL_D3D_TEXTURE_2D_SHARE_HANDLE_ANGLE 路径打开共享纹理，该路径
            // 依赖 legacy GetSharedHandle() 返回的 HANDLE 值，对 NT handle 支持
            // 不稳定（取决于 ANGLE 版本）。
            MiscFlags: D3D11_RESOURCE_MISC_SHARED.0 as u32,
        };

        // ── 创建 D3D11 纹理 ───────────────────────────────────────────────────
        let mut texture: Option<ID3D11Texture2D> = None;
        unsafe {
            self.ctx
                .device()
                .CreateTexture2D(&desc, None, Some(&mut texture))
                .map_err(|e| format!("CreateTexture2D failed ({}x{}): {}", width, height, e))?;
        }
        let texture = texture.ok_or_else(|| "CreateTexture2D returned null".to_string())?;

        // ── 获取 DXGI 共享句柄 ────────────────────────────────────────────────
        // 将纹理转型为 IDXGIResource，通过 GetSharedHandle() 拿到 legacy HANDLE。
        let dxgi_res: IDXGIResource = texture
            .cast()
            .map_err(|e| format!("cast to IDXGIResource failed: {}", e))?;

        let handle = unsafe { dxgi_res.GetSharedHandle() }
            .map_err(|e| format!("GetSharedHandle failed: {}", e))?;

        Ok(TextureSlot {
            texture,
            shared_handle: handle,
            width,
            height,
        })
    }

    /// 创建 D3D11_QUERY_EVENT 查询对象，供 GPU 同步使用。
    ///
    /// 类型 D3D11_QUERY_EVENT 是最轻量的 GPU 同步原语：
    ///   - End()     ：在 GPU 命令队列末尾插入一个「完成标记」；
    ///   - GetData() ：轮询，当 GPU 执行到该标记时将输出 BOOL 置为 TRUE。
    fn create_event_query(ctx: &D3D11Context) -> Result<ID3D11Query, String> {
        let desc = D3D11_QUERY_DESC {
            Query: D3D11_QUERY_EVENT, // 事件查询：等待命令执行完毕
            MiscFlags: 0,
        };
        let mut query: Option<ID3D11Query> = None;
        unsafe {
            ctx.device()
                .CreateQuery(&desc, Some(&mut query))
                .map_err(|e| format!("CreateQuery(D3D11_QUERY_EVENT) failed: {}", e))?;
        }
        query.ok_or_else(|| "CreateQuery returned null ID3D11Query".to_string())
    }

    /// ★ 核心：等待 GPU 真正完成所有已提交命令（包括 UpdateSubresource）。
    ///
    /// 流程：
    ///   1. End(sync_query)    —— 在 GPU 命令队列末尾插入「完成标记」
    ///   2. GetData() 轮询     —— 检查 done 标志；GPU 完成时 D3D11 将 done 置为 TRUE
    ///   3. 退出轮询           —— 此时纹理像素已 100% 写入完成，可安全共享
    ///
    /// 【windows-rs 0.58 的 GetData 行为说明】
    ///   GetData() 在 windows-rs 0.58 中返回 Result<()>，无法直接区分
    ///   S_OK（完成）和 S_FALSE（未完成），因为两者都映射到 Ok(())。
    ///
    ///   解决方案：通过检查输出参数 `done: BOOL` 的值来判断：
    ///     - GPU 完成   (S_OK)   → D3D11 将 done 写为 BOOL(1) → done.as_bool() == true
    ///     - GPU 未完成 (S_FALSE) → done 保持初始值 BOOL(0)   → done.as_bool() == false
    ///
    ///   虽然 MSDN 称 S_FALSE 时 pData 是 undefined，但所有主流 D3D11/WARP
    ///   实现均不会在 S_FALSE 时修改 pData，所以初始化为 0 后检查是可靠的。
    ///
    /// 性能：GPU 执行 UpdateSubresource 通常只需几十微秒（<<1ms），
    ///       自旋等待对实时性能几乎没有影响。
    ///
    /// 降级安全：
    ///   - 若 GPU 设备丢失，GetData 返回 Err，立即退出循环，不会死循环。
    ///   - 硬解/软解切换不影响此函数：只负责 GPU 命令同步，与解码器无关。
    unsafe fn wait_gpu_write_done(&self) {
        // 将 ID3D11Query 转型为 ID3D11Asynchronous（D3D11 父接口）。
        // cast() = COM QueryInterface，轻量操作。
        // 若转型失败（理论上不应发生），跳过同步（保持播放优先，不崩溃）。
        let async_obj: ID3D11Asynchronous = match self.sync_query.cast() {
            Ok(a) => a,
            Err(_) => return, // 降级：跳过本帧同步，接受可能的偶发花屏
        };

        // 在当前 GPU 命令队列末尾插入「结束标记」。
        // 当 GPU 流水线执行到此处时，意味着之前的 UpdateSubresource 写入已完成。
        self.ctx.immediate_context().End(&async_obj);

        // ── GPU 完成轮询 ──────────────────────────────────────────────────────
        // done 初始化为 BOOL(0)（false）：
        //   GPU 完成   → D3D11 将 done 写为 BOOL(1) → done.as_bool() == true  → 退出
        //   GPU 未完成 → done 保持 BOOL(0)          → done.as_bool() == false → 继续
        //   设备丢失   → GetData 返回 Err           → 立即退出，避免死循环
        let mut done = BOOL(0);
        let data_ptr: *mut c_void = std::ptr::addr_of_mut!(done) as *mut c_void;
        let data_size = std::mem::size_of::<BOOL>() as u32;

        loop {
            match self
                .ctx
                .immediate_context()
                .GetData(&async_obj, Some(data_ptr), data_size, 0)
            {
                Ok(()) => {
                    if done.as_bool() {
                        // done = TRUE：GPU 确认写入完成，纹理可安全交给 Flutter/ANGLE
                        break;
                    }
                    // done = FALSE：对应 S_FALSE（GPU 还在处理）
                    // spin_loop 提示 CPU 暂停一个微周期，节电同时让超线程有机会运行
                    std::hint::spin_loop();
                }
                Err(_) => {
                    // 设备丢失（DXGI_ERROR_DEVICE_REMOVED）或其他致命错误。
                    // 立即退出，上层会在后续帧检测到设备失效并进行重建/降级。
                    break;
                }
            }
        }
    }
}





