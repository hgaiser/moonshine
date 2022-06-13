#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

pub const NVFBC_VERSION: u32 = NVFBC_VERSION_MINOR | (NVFBC_VERSION_MAJOR << 8);
