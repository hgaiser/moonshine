#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

pub const NVFBC_VERSION: u32 = NVFBC_VERSION_MINOR | (NVFBC_VERSION_MAJOR << 8);

pub const NVFBC_CREATE_HANDLE_PARAMS_VER: u32 = nvfbc_struct_version::<NVFBC_CREATE_HANDLE_PARAMS>(2);
pub const NVFBC_DESTROY_HANDLE_PARAMS_VER: u32 = nvfbc_struct_version::<NVFBC_DESTROY_HANDLE_PARAMS>(1);
pub const NVFBC_GET_STATUS_PARAMS_VER: u32 = nvfbc_struct_version::<NVFBC_GET_STATUS_PARAMS>(2);
pub const NVFBC_CREATE_CAPTURE_SESSION_PARAMS_VER: u32 = nvfbc_struct_version::<NVFBC_CREATE_CAPTURE_SESSION_PARAMS>(6);
pub const NVFBC_DESTROY_CAPTURE_SESSION_PARAMS_VER: u32 = nvfbc_struct_version::<NVFBC_DESTROY_CAPTURE_SESSION_PARAMS>(1);
pub const NVFBC_TOGL_SETUP_PARAMS_VER: u32 = nvfbc_struct_version::<NVFBC_TOGL_SETUP_PARAMS>(2);
pub const NVFBC_TOGL_GRAB_FRAME_PARAMS_VER: u32 = nvfbc_struct_version::<NVFBC_TOGL_GRAB_FRAME_PARAMS>(2);
pub const NVFBC_TOCUDA_SETUP_PARAMS_VER: u32 = nvfbc_struct_version::<NVFBC_TOCUDA_SETUP_PARAMS>(1);
pub const NVFBC_TOCUDA_GRAB_FRAME_PARAMS_VER: u32 = nvfbc_struct_version::<NVFBC_TOCUDA_GRAB_FRAME_PARAMS>(2);

pub const fn nvfbc_struct_version<T>(version: u32) -> u32 {
	std::mem::size_of::<T>() as u32 | ((version) << 16 | NVFBC_VERSION << 24)
}
