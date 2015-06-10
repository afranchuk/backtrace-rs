use libc::c_void;

pub trait Symbol {
    fn name(&self) -> Option<&[u8]>;
    fn addr(&self) -> Option<*mut c_void>;
}

cascade! {
    if #[cfg(feature = "libbacktrace")] {
    } else if #[cfg(feature = "dladdr")] {
        mod dladdr;
        pub use self::dladdr::resolve;
    } else {
    }
}

