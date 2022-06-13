use nvfbc_sys::{NVFBC_VERSION, NVFBC_API_FUNCTION_LIST, _NVFBCSTATUS_NVFBC_SUCCESS, NVFBCSTATUS};

fn main() {
	unsafe {
		let lib = libloading::Library::new("libnvidia-fbc.so").unwrap();
		let nvfbc_create_instance: libloading::Symbol<unsafe extern fn(*mut NVFBC_API_FUNCTION_LIST) -> NVFBCSTATUS> = lib.get(b"NvFBCCreateInstance").unwrap();

		let mut nvfbc_api_function_list: NVFBC_API_FUNCTION_LIST = NVFBC_API_FUNCTION_LIST {
			dwVersion: NVFBC_VERSION,
			nvFBCGetLastErrorStr: None,
			nvFBCCreateHandle: None,
			nvFBCDestroyHandle: None,
			nvFBCGetStatus: None,
			nvFBCCreateCaptureSession: None,
			nvFBCDestroyCaptureSession: None,
			nvFBCToSysSetUp: None,
			nvFBCToSysGrabFrame: None,
			nvFBCToCudaSetUp: None,
			nvFBCToCudaGrabFrame: None,
			pad1: std::ptr::null_mut(),
			pad2: std::ptr::null_mut(),
			pad3: std::ptr::null_mut(),
			nvFBCBindContext: None,
			nvFBCReleaseContext: None,
			pad4: std::ptr::null_mut(),
			pad5: std::ptr::null_mut(),
			pad6: std::ptr::null_mut(),
			pad7: std::ptr::null_mut(),
			nvFBCToGLSetUp: None,
			nvFBCToGLGrabFrame: None
		};
		let ret = nvfbc_create_instance(&mut nvfbc_api_function_list as *mut NVFBC_API_FUNCTION_LIST);
		if ret != _NVFBCSTATUS_NVFBC_SUCCESS {
			eprintln!("Unable to create NvFBC instance");
		}

		println!("{:#?}", ret);

	}
}
