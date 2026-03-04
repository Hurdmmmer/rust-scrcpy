use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::error::{Result, ScrcpyError};

/// 设备画像快照。
///
/// 该快照与 Session 层解耦，专门用于落盘缓存，
/// 避免 session 结构变更时直接破坏缓存格式。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DeviceProfileSnapshot {
    pub device_id: String,
    pub model: String,
    pub android_version: String,
    pub screen_width: u32,
    pub screen_height: u32,
}

/// 设备缓存容器。
///
/// 键为设备序列号，值为最近一次建链成功时采集到的设备信息。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DeviceCache {
    pub devices: HashMap<String, DeviceProfileSnapshot>,
}

impl DeviceCache {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let content = fs::read_to_string(path)
            .map_err(|e| ScrcpyError::Other(format!("读取设备缓存失败: {}", e)))?;

        let cache = serde_json::from_str::<Self>(&content)
            .map_err(|e| ScrcpyError::Other(format!("解析设备缓存失败: {}", e)))?;

        Ok(cache)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                fs::create_dir_all(parent)
                    .map_err(|e| ScrcpyError::Other(format!("创建设备缓存目录失败: {}", e)))?;
            }
        }

        let json = serde_json::to_string_pretty(self)
            .map_err(|e| ScrcpyError::Other(format!("序列化设备缓存失败: {}", e)))?;

        fs::write(path, json)
            .map_err(|e| ScrcpyError::Other(format!("写入设备缓存失败: {}", e)))?;

        Ok(())
    }

    pub fn get(&self, device_id: &str) -> Option<&DeviceProfileSnapshot> {
        self.devices.get(device_id)
    }

    pub fn upsert(&mut self, profile: DeviceProfileSnapshot) {
        self.devices.insert(profile.device_id.clone(), profile);
    }
}
