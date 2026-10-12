//! Minimal access to the UPS through the stock Windows HID driver.

use std::io;
use std::mem::size_of;
use std::ptr::{null, null_mut};
use std::thread::sleep;
use std::time::Duration;

use windows_sys::core::GUID;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
};
use windows_sys::Win32::Devices::HumanInterfaceDevice::{
    HidD_GetAttributes, HidD_GetHidGuid, HidD_GetIndexedString, HIDD_ATTRIBUTES,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_SHARING_VIOLATION, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

pub const VENDOR_ID: u16 = 0x0001;
pub const PRODUCT_ID: u16 = 0x0000;

pub struct Ups {
    handle: HANDLE,
}

// The handle is only used for synchronous HidD_* calls, which are thread-safe.
unsafe impl Send for Ups {}

impl Drop for Ups {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.handle) };
    }
}

impl Ups {
    /// Opens the first present HID device with the UPS VID/PID.
    pub fn open() -> io::Result<Ups> {
        let mut sharing_violation = false;
        for path in hid_device_paths()? {
            let handle = unsafe {
                CreateFileW(
                    path.as_ptr(),
                    0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    null(),
                    OPEN_EXISTING,
                    0,
                    null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                if unsafe { GetLastError() } == ERROR_SHARING_VIOLATION {
                    sharing_violation = true;
                }
                continue;
            }
            let mut attrs: HIDD_ATTRIBUTES = unsafe { std::mem::zeroed() };
            attrs.Size = size_of::<HIDD_ATTRIBUTES>() as u32;
            let ok = unsafe { HidD_GetAttributes(handle, &mut attrs) } != 0;
            if ok && attrs.VendorID == VENDOR_ID && attrs.ProductID == PRODUCT_ID {
                return Ok(Ups { handle });
            }
            unsafe { CloseHandle(handle) };
        }
        if sharing_violation {
            Err(io::Error::new(
                io::ErrorKind::ResourceBusy,
                "ДБЖ зайнятий іншою програмою (зупиніть upsSmartServer)",
            ))
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "ДБЖ (USB 0001:0000) не знайдено",
            ))
        }
    }

    /// Issues one command (string descriptor request) and returns the reply,
    /// retrying failed requests. Not for toggles: a retry would undo them.
    pub fn command(&self, index: u32) -> io::Result<String> {
        let mut last_err = io::Error::other("no attempt made");
        for attempt in 0..3 {
            if attempt > 0 {
                sleep(Duration::from_millis(100));
            }
            match self.command_once(index) {
                Ok(reply) => return Ok(reply),
                Err(e) => last_err = e,
            }
        }
        Err(last_err)
    }

    /// Issues one command exactly once.
    pub fn command_once(&self, index: u32) -> io::Result<String> {
        let mut buf = [0u16; 128];
        let ok = unsafe {
            HidD_GetIndexedString(
                self.handle,
                index,
                buf.as_mut_ptr().cast(),
                (buf.len() * 2) as u32,
            )
        } != 0;
        if ok {
            let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            Ok(String::from_utf16_lossy(&buf[..len]))
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

fn hid_device_paths() -> io::Result<Vec<Vec<u16>>> {
    let needle = format!("vid_{VENDOR_ID:04x}&pid_{PRODUCT_ID:04x}");
    let mut guid: GUID = unsafe { std::mem::zeroed() };
    unsafe { HidD_GetHidGuid(&mut guid) };
    let devs = unsafe {
        SetupDiGetClassDevsW(
            &guid,
            null(),
            null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
    };
    if devs as isize == -1 {
        return Err(io::Error::last_os_error());
    }
    let mut paths = Vec::new();
    for i in 0.. {
        let mut data: SP_DEVICE_INTERFACE_DATA = unsafe { std::mem::zeroed() };
        data.cbSize = size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
        if unsafe { SetupDiEnumDeviceInterfaces(devs, null(), &guid, i, &mut data) } == 0 {
            break;
        }
        let mut needed = 0u32;
        unsafe {
            SetupDiGetDeviceInterfaceDetailW(devs, &data, null_mut(), 0, &mut needed, null_mut())
        };
        if needed < 8 {
            continue;
        }
        // u32 backing keeps the struct aligned.
        let mut buf = vec![0u32; (needed as usize).div_ceil(4)];
        let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
        unsafe { (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32 };
        let ok = unsafe {
            SetupDiGetDeviceInterfaceDetailW(devs, &data, detail, needed, null_mut(), null_mut())
        } != 0;
        if !ok {
            continue;
        }
        let path_ptr = unsafe { std::ptr::addr_of!((*detail).DevicePath) } as *const u16;
        let max = (needed as usize - 4) / 2;
        let path: Vec<u16> = (0..max)
            .map(|k| unsafe { *path_ptr.add(k) })
            .take_while(|&c| c != 0)
            .collect();
        if String::from_utf16_lossy(&path)
            .to_lowercase()
            .contains(&needle)
        {
            let mut p = path;
            p.push(0);
            paths.push(p);
        }
    }
    unsafe { SetupDiDestroyDeviceInfoList(devs) };
    Ok(paths)
}
