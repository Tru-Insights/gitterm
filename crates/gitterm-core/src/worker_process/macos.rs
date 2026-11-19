use super::ProcInfo;
use std::ffi::c_void;

fn errno() -> std::io::Error {
    std::io::Error::last_os_error()
}

pub fn child_pids(pid: u32) -> Result<Vec<u32>, String> {
    let mut cap = 64usize;
    loop {
        let mut buf = vec![0 as libc::pid_t; cap];
        let size = (cap * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        // SAFETY: the buffer holds `size` bytes; the call writes at most
        // that many and returns how many pids it wrote.
        let n = unsafe {
            libc::proc_listchildpids(pid as libc::pid_t, buf.as_mut_ptr() as *mut c_void, size)
        };
        if n < 0 {
            return Err(format!("proc_listchildpids({pid}): {}", errno()));
        }
        let n = n as usize;
        if n < cap {
            buf.truncate(n);
            return Ok(buf
                .into_iter()
                .filter(|p| *p > 0)
                .map(|p| p as u32)
                .collect());
        }
        cap *= 4;
        if cap > 1 << 16 {
            return Err(format!("proc_listchildpids({pid}): too many children"));
        }
    }
}

/// `None` when the process is gone.
pub fn info(pid: u32) -> Result<Option<ProcInfo>, String> {
    // SAFETY: proc_bsdinfo is plain data; zeroed is a valid value.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: `info` is `size` bytes and the flavor writes that struct.
    let n = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut c_void,
            size,
        )
    };
    if n != size {
        let error = errno();
        return match error.raw_os_error() {
            Some(libc::ESRCH) | Some(0) | None => Ok(None),
            _ => Err(format!("proc_pidinfo({pid}, PROC_PIDTBSDINFO): {error}")),
        };
    }
    let comm: Vec<u8> = info
        .pbi_comm
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect();
    Ok(Some(ProcInfo {
        pid,
        ppid: info.pbi_ppid,
        comm: String::from_utf8_lossy(&comm).into_owned(),
        start_us: info.pbi_start_tvsec as i64 * 1_000_000 + info.pbi_start_tvusec as i64,
    }))
}

pub fn child_info(parent: u32, pid: u32) -> Result<Option<ProcInfo>, String> {
    // The enumerated pid may have exited and been reused before info().
    Ok(info(pid)?.filter(|child| child.ppid == parent))
}

pub fn descendants(pid: u32) -> Result<Vec<ProcInfo>, String> {
    let mut out = Vec::new();
    let mut queue: Vec<_> = info(pid)?.into_iter().collect();
    while let Some(parent) = queue.pop() {
        if info(parent.pid)?.as_ref() != Some(&parent) {
            continue;
        }
        let children: Vec<_> = child_pids(parent.pid)?
            .into_iter()
            .map(|child| child_info(parent.pid, child))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect();
        // A reused intermediate pid must not introduce another tree.
        if info(parent.pid)?.as_ref() != Some(&parent) {
            continue;
        }
        queue.extend(children.iter().cloned());
        out.extend(children);
        if out.len() > 4096 {
            return Err(format!("more than 4096 processes under {pid}"));
        }
    }
    Ok(out)
}
