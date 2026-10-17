//! Join the real phone hotspot using a normal per-user Windows WLAN profile.
//! Never put credentials in process arguments, temporary files, or logs.
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};
use windows::core::GUID;

pub struct Credentials {
    pub ssid: String,
    pub password: String,
    pub security: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HotspotCredentials([redacted])")
    }
}

impl Credentials {
    pub fn validate(&self) -> bool {
        !self.ssid.is_empty() && self.ssid.len() <= 32 &&
        match self.security.as_str() {
            "open" => self.password.is_empty(),
            "wpa2" | "wpa2-wpa3" | "wpa3" => (8..=63).contains(&self.password.len()),
            _ => false,
        }
    }
}

#[repr(C)]
struct Interface { guid: GUID, _description: [u16; 256], state: u32 }
#[repr(C)]
struct InterfaceList { count: u32, _index: u32, first: Interface }
#[repr(C)]
struct ConnectionParameters {
    mode: u32,
    profile: *const u16,
    ssid: *const c_void,
    bssid_list: *const c_void,
    bss_type: u32,
    flags: u32,
}

#[link(name = "wlanapi")]
extern "system" {
    fn WlanOpenHandle(version: u32, reserved: *const c_void, negotiated: *mut u32, handle: *mut *mut c_void) -> u32;
    fn WlanCloseHandle(handle: *mut c_void, reserved: *const c_void) -> u32;
    fn WlanEnumInterfaces(handle: *mut c_void, reserved: *const c_void, list: *mut *mut InterfaceList) -> u32;
    fn WlanFreeMemory(memory: *mut c_void);
    fn WlanConnect(handle: *mut c_void, guid: *const GUID, params: *const ConnectionParameters, reserved: *const c_void) -> u32;
    fn WlanSetProfile(handle: *mut c_void, guid: *const GUID, flags: u32, xml: *const u16,
        security: *const u16, overwrite: i32, reserved: *const c_void, reason: *mut u32) -> u32;
    fn WlanQueryInterface(handle: *mut c_void, guid: *const GUID, opcode: u32, reserved: *const c_void,
        size: *mut u32, data: *mut *mut c_void, value_type: *mut u32) -> u32;
}

struct Handle(*mut c_void);
impl Drop for Handle {
    fn drop(&mut self) { unsafe { WlanCloseHandle(self.0, null()); } }
}
struct WlanMemory(*mut c_void);
impl Drop for WlanMemory {
    fn drop(&mut self) { unsafe { WlanFreeMemory(self.0); } }
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
        .replace('"', "&quot;").replace('\'', "&apos;")
}

fn profile(c: &Credentials) -> String {
    let name = xml_escape(&c.ssid);
    let (authentication, encryption) = match c.security.as_str() {
        "open" => ("open", "none"),
        "wpa3" => ("WPA3SAE", "AES"),
        _ => ("WPA2PSK", "AES"), // transition mode supports WPA2 clients
    };
    let key = if c.security == "open" { String::new() } else {
        format!("<sharedKey><keyType>passPhrase</keyType><protected>false</protected><keyMaterial>{}</keyMaterial></sharedKey>", xml_escape(&c.password))
    };
    format!(r#"<?xml version="1.0"?><WLANProfile xmlns="http://www.microsoft.com/networking/WLAN/profile/v1"><name>{name}</name><SSIDConfig><SSID><hex>{}</hex></SSID></SSIDConfig><connectionType>ESS</connectionType><connectionMode>manual</connectionMode><MSM><security><authEncryption><authentication>{authentication}</authentication><encryption>{encryption}</encryption><useOneX>false</useOneX></authEncryption>{key}</security></MSM></WLANProfile>"#, hex::encode(c.ssid.as_bytes()))
}

/// Native query returns WLAN_CONNECTION_ATTRIBUTES. Inspect only its fixed header + DOT11_SSID.
unsafe fn connected(handle: *mut c_void, guid: &GUID, ssid: &[u8], profile_name: &str) -> bool {
    unsafe {
    let (mut size, mut data, mut kind) = (0, null_mut(), 0);
    if WlanQueryInterface(handle, guid, 7, null(), &mut size, &mut data, &mut kind) != 0 || data.is_null() {
        return false;
    }
    let _memory = WlanMemory(data);
    // state (4), connection mode (4), WCHAR profile name[256], DOT11_SSID (length + bytes[32]).
    if size < 556 { return false; }
    let bytes = std::slice::from_raw_parts(data as *const u8, size as usize);
    let state = u32::from_ne_bytes(bytes[0..4].try_into().unwrap());
    let len = u32::from_ne_bytes(bytes[520..524].try_into().unwrap()) as usize;
    let active_profile: Vec<u16> = bytes[8..520].chunks_exact(2)
        .map(|b| u16::from_ne_bytes([b[0], b[1]])).take_while(|c| *c != 0).collect();
    state == 1 && len <= 32 && &bytes[524..524 + len] == ssid &&
        active_profile == profile_name.encode_utf16().collect::<Vec<_>>()
    }
}

/// Errors are deliberately generic: never forward native profile/credential content to logs.
pub fn join(credentials: Credentials) -> Result<(), String> {
    if !credentials.validate() { return Err("Invalid or unsupported phone hotspot configuration".into()); }
    unsafe {
        let (mut version, mut raw) = (0, null_mut());
        let code = WlanOpenHandle(2, null(), &mut version, &mut raw);
        if code != 0 { return Err(format!("Windows Wi-Fi service unavailable (code {code})")); }
        let handle = Handle(raw);
        let mut list = null_mut();
        let code = WlanEnumInterfaces(handle.0, null(), &mut list);
        if code != 0 || list.is_null() { return Err(format!("Could not enumerate Wi-Fi adapters (code {code})")); }
        let memory = WlanMemory(list as *mut c_void);
        let count = (*list).count as usize;
        if count == 0 || count > 64 { return Err("No Wi-Fi adapter available".into()); }
        let interfaces = std::slice::from_raw_parts(std::ptr::addr_of!((*list).first), count);
        let guids: Vec<GUID> = interfaces.iter().filter(|i| i.state != 0).map(|i| i.guid).collect();
        drop(memory);
        if guids.is_empty() { return Err("Turn this PC's Wi-Fi on and try again".into()); }
        let profile_name = &credentials.ssid;
        let xml: Vec<u16> = profile(&credentials).encode_utf16().chain(Some(0)).collect();
        let name: Vec<u16> = profile_name.encode_utf16().chain(Some(0)).collect();
        let params = ConnectionParameters { mode: 0, profile: name.as_ptr(), ssid: null(), bssid_list: null(), bss_type: 1, flags: 0 };
        // Normal saved profile, named exactly after the real SSID. Windows protects the
        // stored key; no plaintext file or fabricated temporary network is created.
        let mut accepted = false;
        let mut last_error = 0;
        for guid in &guids {
            let mut reason = 0;
            // WLAN_PROFILE_USER = 2. Let the system manage connect/disconnect normally.
            let mut code = WlanSetProfile(handle.0, guid, 2, xml.as_ptr(), null(), 1, null(), &mut reason);
            // A same-name all-user profile cannot be overwritten as a per-user
            // profile (ERROR_ALREADY_EXISTS). Update it in its original scope.
            if code == 183 {
                code = WlanSetProfile(handle.0, guid, 0, xml.as_ptr(), null(), 1, null(), &mut reason);
                if code != 0 {
                    // Existing system-managed profiles may be read-only. Try that
                    // real-SSID profile directly rather than inventing another name.
                    code = WlanConnect(handle.0, guid, &params, null());
                    if code == 0 { accepted = true; break; }
                }
            }
            if code != 0 { last_error = code; continue; }
            let code = WlanConnect(handle.0, guid, &params, null());
            if code == 0 { accepted = true; break; }
            last_error = code;
        }
        if !accepted {
            return Err(format!("Windows could not start the Wi-Fi connection (code {last_error}); check Wi-Fi/location permissions and security support"));
        }
        let deadline = Instant::now() + Duration::from_secs(40);
        while Instant::now() < deadline {
            if guids.iter().any(|guid| connected(handle.0, guid, credentials.ssid.as_bytes(), &profile_name)) { return Ok(()); }
            std::thread::sleep(Duration::from_millis(500));
        }
        Err("Could not join the phone hotspot; check Wi-Fi, signal, location permission and WPA3 support. The phone hotspot may still be on".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn redacts_debug_and_escapes_profile() {
        let c = Credentials { ssid: "Phone<&".into(), password: "secret<&123".into(), security: "wpa2".into() };
        assert!(c.validate());
        assert!(!format!("{c:?}").contains(&c.password));
        let p = profile(&c);
        assert!(p.contains("secret&lt;&amp;123"));
        assert!(p.contains(&hex::encode(c.ssid.as_bytes())));
        assert!(p.contains("<name>Phone&lt;&amp;</name>"));
        assert!(!p.contains("BridgeHotspot"));
    }
}
