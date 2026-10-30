use std::fs::File;
use std::os::fd::AsRawFd;
use td_time::request::{format_utc, MAX_EPOCH, MIN_EPOCH};

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("td-time requires Linux x86-64");

const CLOCK_SETTIME: usize = 227;
const CLOCK_REALTIME: usize = 0;
const IOCTL: usize = 16;
const RTC_SET_TIME: usize = 0x4024700a;
const _: () = assert!(std::mem::size_of::<[i64; 2]>() == 16);
const _: () = assert!(std::mem::size_of::<[i32; 9]>() == 36);

#[allow(unsafe_code)]
fn syscall3(number: usize, first: usize, second: usize, third: usize) -> Result<(), String> {
    let result: isize;
    unsafe {
        std::arch::asm!(
            "syscall",
            inlateout("rax") number => result,
            in("rdi") first,
            in("rsi") second,
            in("rdx") third,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    if result < 0 {
        Err(std::io::Error::from_raw_os_error((-result) as i32).to_string())
    } else if result == 0 {
        Ok(())
    } else {
        Err("unexpected clock syscall result".into())
    }
}

pub(crate) fn realtime(milliseconds: u64) -> Result<(), String> {
    let seconds = milliseconds / 1000;
    if !(MIN_EPOCH..=MAX_EPOCH).contains(&seconds) {
        return Err("clock time outside supported range".into());
    }
    let time = [seconds as i64, ((milliseconds % 1000) * 1_000_000) as i64];
    syscall3(CLOCK_SETTIME, CLOCK_REALTIME, time.as_ptr() as usize, 0)
}

pub(crate) fn rtc_fields(seconds: u64) -> Result<[i32; 9], String> {
    let utc = format_utc(seconds)?;
    let number = |start, end| -> Result<i32, String> {
        utc.get(start..end)
            .ok_or("RTC date field")?
            .parse()
            .map_err(|_| "RTC date value".into())
    };
    Ok([
        number(17, 19)?,
        number(14, 16)?,
        number(11, 13)?,
        number(8, 10)?,
        number(5, 7)? - 1,
        number(0, 4)? - 1900,
        0,
        0,
        0,
    ])
}

pub(crate) fn rtc(file: &File, seconds: u64) -> Result<(), String> {
    let time = rtc_fields(seconds)?;
    syscall3(
        IOCTL,
        file.as_raw_fd() as usize,
        RTC_SET_TIME,
        time.as_ptr() as usize,
    )
}
