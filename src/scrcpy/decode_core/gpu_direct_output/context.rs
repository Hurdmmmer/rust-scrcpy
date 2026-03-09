use windows::core::Result as WinResult;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_WARP;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
    ID3D11DeviceContext,
};

/// 真实 D3D11 上下文。
#[derive(Clone)]
pub struct D3D11Context {
    device: ID3D11Device,
    immediate_context: ID3D11DeviceContext,
}

impl D3D11Context {
    pub fn new() -> Result<Self, String> {
        match Self::create_with_driver(D3D_DRIVER_TYPE_HARDWARE) {
            Ok(v) => Ok(v),
            Err(hw_err) => {
                // 硬件设备创建失败时回退到 WARP，保证链路可运行。
                Self::create_with_driver(D3D_DRIVER_TYPE_WARP).map_err(|warp_err| {
                    format!(
                        "create D3D11 context failed: hw={}, warp={}",
                        hw_err, warp_err
                    )
                })
            }
        }
    }

    fn create_with_driver(
        driver_type: windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE,
    ) -> Result<Self, String> {
        let mut device: Option<ID3D11Device> = None;
        let mut immediate_context: Option<ID3D11DeviceContext> = None;

        let create_result: WinResult<()> = unsafe {
            D3D11CreateDevice(
                None,
                driver_type,
                None,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut immediate_context),
            )
        };

        create_result.map_err(|e| format!("D3D11CreateDevice failed: {}", e))?;

        let device = device.ok_or_else(|| "D3D11 device is null".to_string())?;
        let immediate_context =
            immediate_context.ok_or_else(|| "D3D11 immediate context is null".to_string())?;

        Ok(Self {
            device,
            immediate_context,
        })
    }

    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    pub fn immediate_context(&self) -> &ID3D11DeviceContext {
        &self.immediate_context
    }
}



