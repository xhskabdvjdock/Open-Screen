//! Media Foundation hardware-encoder detection (no FFmpeg).
//!
//! Enumerates video-encoder MFTs with `MFT_ENUM_FLAG_HARDWARE` for H.264 and
//! HEVC. Used for the honest Advanced-settings label (`Hardware — NVIDIA` vs
//! `Software`) and for gating codec offers (never offer what isn't there).

use serde::Serialize;

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HwEncoders {
    pub h264_hw: Vec<String>,
    pub hevc_hw: Vec<String>,
}

impl HwEncoders {
    pub fn vendor(&self) -> Option<String> {
        for n in self.h264_hw.iter().chain(self.hevc_hw.iter()) {
            let l = n.to_lowercase();
            if l.contains("nvenc") || l.contains("nvidia") {
                return Some("NVIDIA".to_string());
            }
            if l.contains("intel") || l.contains("quick") || l.contains("qsv") {
                return Some("Intel".to_string());
            }
            if l.contains("amd") || l.contains("amf") {
                return Some("AMD".to_string());
            }
        }
        None
    }
}

#[cfg(target_os = "windows")]
mod inner {
    use super::*;
    use windows::Win32::Media::MediaFoundation::{
        MFStartup, MFVideoFormat_H264, MFVideoFormat_HEVC, MFVideoFormat_NV12,
        MFMediaType_Video, MFTEnumEx, MFT_CATEGORY_VIDEO_ENCODER,
        MFT_ENUM_FLAG_HARDWARE, MFT_FRIENDLY_NAME_Attribute, MFSTARTUP_FULL,
        MF_VERSION, MFT_REGISTER_TYPE_INFO,
    };
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::core::PWSTR;

    fn enum_hw(subtype: &windows::core::GUID) -> Vec<String> {
        let mut out = vec![];
        unsafe {
            // MFStartup is ref-counted; never MFShutdown here (the encoder
            // pipeline shares the platform lifetime with this process).
            if MFStartup(MF_VERSION, MFSTARTUP_FULL).is_err() {
                return out;
            }
            // Encoders take UNCOMPRESSED input (NV12) and emit the codec:
            // filtering input=H264 would only match decoders/transcoders.
            let in_info = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_NV12,
            };
            let out_info = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: *subtype,
            };
            let mut activates: *mut Option<windows::Win32::Media::MediaFoundation::IMFActivate> = std::ptr::null_mut();
            let mut count = 0u32;
            // pppMFTActivate type: *mut *mut IMFActivate — pass through.
            let hr = MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_HARDWARE,
                Some(&in_info),
                Some(&out_info),
                &mut activates as *mut _ as *mut *mut _,
                &mut count,
            );
            if hr.is_err() {
                return out;
            }
            if !activates.is_null() {
                let slice = std::slice::from_raw_parts(activates, count as usize);
                for act in slice.iter().flatten() {
                    let mut len = 0u32;
                    let mut name = PWSTR::null();
                    if act
                        .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut name, &mut len)
                        .is_ok()
                        && !name.is_null()
                    {
                        let s = name.to_string().unwrap_or_default();
                        if !s.trim().is_empty() {
                            out.push(s);
                        }
                        CoTaskMemFree(Some(name.as_ptr() as *const _));
                    }
                    let _ = act.ShutdownObject();
                }
                CoTaskMemFree(Some(activates as *const _));
            }
        }
        out.sort();
        out.dedup();
        out
    }

    pub fn probe_hw_encoders() -> HwEncoders {
        HwEncoders {
            h264_hw: enum_hw(&MFVideoFormat_H264),
            hevc_hw: enum_hw(&MFVideoFormat_HEVC),
        }
    }

    /// Software encoder MFT names for a codec (sanity/diagnostics + codec
    /// gating: offer HEVC only when an HEVC MFT really exists).
    pub fn probe_sw(codec: &str) -> Vec<String> {
        match codec.to_lowercase().as_str() {
            "hevc" | "h265" => probe_sw_subtype(&MFVideoFormat_HEVC),
            _ => probe_sw_subtype(&MFVideoFormat_H264),
        }
    }

    fn probe_sw_subtype(subtype: &windows::core::GUID) -> Vec<String> {
        use windows::Win32::Media::MediaFoundation::MFT_ENUM_FLAG_SYNCMFT;
        let mut out = vec![];
        unsafe {
            if MFStartup(MF_VERSION, MFSTARTUP_FULL).is_err() {
                return out;
            }
            let info_in = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_NV12,
            };
            let info = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: *subtype,
            };
            let mut activates: *mut Option<
                windows::Win32::Media::MediaFoundation::IMFActivate,
            > = std::ptr::null_mut();
            let mut count = 0u32;
            if MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_SYNCMFT,
                Some(&info_in),
                Some(&info),
                &mut activates as *mut _ as *mut *mut _,
                &mut count,
            )
            .is_err()
            {
                return out;
            }
            if !activates.is_null() {
                let slice = std::slice::from_raw_parts(activates, count as usize);
                for act in slice.iter().flatten() {
                    let mut len = 0u32;
                    let mut name = PWSTR::null();
                    if act
                        .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut name, &mut len)
                        .is_ok()
                        && !name.is_null()
                    {
                        out.push(name.to_string().unwrap_or_default());
                        CoTaskMemFree(Some(name.as_ptr() as *const _));
                    }
                    let _ = act.ShutdownObject();
                }
                CoTaskMemFree(Some(activates as *const _));
            }
        }
        out.sort();
        out.dedup();
        out
    }
}

#[cfg(target_os = "windows")]
pub use inner::{probe_hw_encoders, probe_sw};

#[cfg(not(target_os = "windows"))]
pub fn probe_hw_encoders() -> HwEncoders {
    HwEncoders::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "windows")]
    fn mf_hw_probe_runs() {
        let hw = probe_hw_encoders();
        eprintln!("HW encoders: {hw:?}");
        let sw = probe_sw("h264");
        eprintln!("SW H264 MFTs: {sw:?}");
        // Enumeration itself must work (inbox Microsoft H264 encoder MFT).
        assert!(!sw.is_empty(), "MFT enumeration broken: no SW H264 MFT");
        // HW presence is hardware-dependent — report honestly, don't assert.
    }
}
