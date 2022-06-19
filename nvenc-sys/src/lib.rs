#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

// pub const NVENCAPI_VERSION: u32 = NVENCAPI_MAJOR_VERSION | (NVENCAPI_MINOR_VERSION << 24);

// pub const fn nvenc_struct_version<T>(version: u32) -> u32 {
// 	std::mem::size_of::<T>() as u32 | ((version) << 16 | NVENCAPI_VERSION << 24)
// }
