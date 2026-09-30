use crate::{
    sys::{InputtinoDeviceDefinition, InputtinoErrorHandler},
    InputtinoError,
};
use std::{ffi::CString, path::PathBuf};

/// Definition of the type of device to emulate.
#[allow(dead_code)]
pub struct DeviceDefinition {
    pub def: InputtinoDeviceDefinition,
    // Keep those around since we are passing them as pointers
    name: CString,
    phys: CString,
    uniq: CString,
}

impl DeviceDefinition {
    /// Create a new device definition for creating emulated devices.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the emulated device.
    /// * `vendor_id` - ID of the vendor that created the emulated device.
    /// * `product_id` - ID of the product.
    /// * `version` -
    /// * `phys` -
    /// * `uniq` -
    pub fn new(
        name: &str,
        vendor_id: u16,
        product_id: u16,
        version: u16,
        phys: &str,
        uniq: &str,
    ) -> Self {
        let name = CString::new(name).unwrap();
        let phys = CString::new(phys).unwrap();
        let uniq = CString::new(uniq).unwrap();
        let def = InputtinoDeviceDefinition {
            name: name.as_ptr(),
            vendor_id,
            product_id,
            version,
            device_phys: phys.as_ptr(), // TODO: optional, if not present random MAC address
            device_uniq: uniq.as_ptr(),
        };

        DeviceDefinition {
            def,
            name,
            phys,
            uniq,
        }
    }
}

pub(crate) unsafe extern "C" fn error_handler_fn(
    error_message: *const ::core::ffi::c_char,
    user_data: *mut ::core::ffi::c_void,
) {
    let error_str = std::ffi::CStr::from_ptr(error_message);
    let user_data = user_data as *mut CString;
    *user_data = CString::from(error_str);
}

type GetNodesFn<T> = unsafe extern "C" fn(
    device: *mut T,
    num_nodes: *mut ::core::ffi::c_int,
) -> *mut *mut ::core::ffi::c_char;

pub(crate) fn get_nodes<T>(
    f: GetNodesFn<T>,
    device: *mut T,
) -> Result<Vec<PathBuf>, InputtinoError> {
    let mut nodes_count: core::ffi::c_int = 0;
    let nodes = unsafe { f(device, &mut nodes_count) };
    if nodes.is_null() {
        return Err(InputtinoError::Generic("Failed to get nodes".to_string()));
    }

    let mut result = Vec::new();
    for i in 0..nodes_count {
        let node = unsafe { std::ffi::CString::from_raw(*nodes.offset(i as isize)) };
        result.push(PathBuf::from(&node.to_string_lossy().to_string()));
    }

    Ok(result)
}

type CreateDeviceFn<T> = unsafe extern "C" fn(
    device: *const InputtinoDeviceDefinition,
    eh: *const InputtinoErrorHandler,
) -> *mut T;

pub(crate) fn make_device<T>(
    f: CreateDeviceFn<T>,
    definition: &DeviceDefinition,
) -> Result<*mut T, InputtinoError> {
    let mut error_str = std::ffi::CString::default();
    let error_handler = InputtinoErrorHandler {
        eh: Some(error_handler_fn),
        user_data: &mut error_str as *mut _ as *mut std::ffi::c_void,
    };
    let device = unsafe { f(&definition.def, &error_handler) };
    if device.is_null() {
        let error_msg = error_str.to_string_lossy();
        Err(InputtinoError::Generic(format!(
            "Failed to create input device: {error_msg}"
        )))
    } else {
        Ok(device)
    }
}

unsafe impl Send for DeviceDefinition {}
unsafe impl Sync for DeviceDefinition {}
