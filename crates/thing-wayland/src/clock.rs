use std::{
    ffi::CStr,
    mem::MaybeUninit,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// local wall-clock time, refreshed at minute boundaries rather than every frame.
pub(crate) fn read() -> (String, String, Duration) {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = elapsed.as_secs() as libc::time_t;
    let mut local = MaybeUninit::<libc::tm>::uninit();
    // SAFETY: seconds and local point to valid storage; localtime_r initializes
    // the output on success. neither pointer is retained by libc.
    let valid = unsafe { !libc::localtime_r(&seconds, local.as_mut_ptr()).is_null() };
    let format = |pattern: &CStr, fallback: &str| {
        let mut buffer = [0_u8; 128];
        let length = if valid {
            // SAFETY: localtime_r succeeded, and the output buffer and literal
            // format are valid for this call. strftime writes at most buffer.len().
            unsafe {
                libc::strftime(
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    pattern.as_ptr(),
                    local.as_ptr(),
                )
            }
        } else {
            0
        };
        if length == 0 {
            fallback.into()
        } else {
            String::from_utf8_lossy(&buffer[..length]).into_owned()
        }
    };
    let until_next =
        Duration::from_secs(60) - Duration::new(elapsed.as_secs() % 60, elapsed.subsec_nanos());
    (
        format(c"%H:%M", "--:--"),
        format(c"%a %-d %b", ""),
        until_next,
    )
}
