#![no_std]

#[unsafe(no_mangle)]
pub unsafe extern "C" fn neomacs_cache_memcpy(
    dst: *mut u8,
    src: *const u8,
    len: usize,
) -> *mut u8 {
    let mut i = 0;
    while i < len {
        unsafe { dst.add(i).write_volatile(src.add(i).read_volatile()) };
        i += 1;
    }
    dst
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn neomacs_cache_memmove(
    dst: *mut u8,
    src: *const u8,
    len: usize,
) -> *mut u8 {
    if (dst as usize) <= (src as usize) {
        return unsafe { neomacs_cache_memcpy(dst, src, len) };
    }
    let mut i = len;
    while i != 0 {
        i -= 1;
        unsafe { dst.add(i).write_volatile(src.add(i).read_volatile()) };
    }
    dst
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn neomacs_cache_memset(
    dst: *mut u8,
    value: i32,
    len: usize,
) -> *mut u8 {
    let mut i = 0;
    while i < len {
        unsafe { dst.add(i).write_volatile(value as u8) };
        i += 1;
    }
    dst
}
