//! Lexical Windows path normalization for the named-pipe identity.
//!
//! This is `path.win32.resolve(cwd, home)`: absolute, `.` and `..` removed,
//! repeated separators collapsed. It does not call `GetFinalPathName` /
//! `realpath` and does not expand junctions or symlinks. Drive-relative
//! paths (`D:rel`) whose drive is not `cwd` resolve at that drive's root,
//! which is what Node does on Windows when no per-drive cwd is set.

fn is_sep(b: u8) -> bool {
    b == b'\\' || b == b'/'
}

fn is_device(b: u8) -> bool {
    b.is_ascii_alphabetic()
}

/// `normalizeString` from Node's win32 path implementation.
fn normalize_string(path: &str, allow_above_root: bool) -> String {
    let bytes = path.as_bytes();
    let mut res = String::new();
    let mut last_segment_length = 0usize;
    let mut last_slash: isize = -1;
    let mut dots: i32 = 0;
    let mut code: u8 = 0;
    let mut i = 0usize;
    loop {
        if i < bytes.len() {
            code = bytes[i];
        } else if is_sep(code) {
            break;
        } else {
            code = b'/';
        }
        if is_sep(code) {
            if last_slash == i as isize - 1 || dots == 1 {
                // skip empty segments and "."
            } else if dots == 2 {
                let pop = res.len() < 2
                    || last_segment_length != 2
                    || res.as_bytes()[res.len() - 1] != b'.'
                    || res.as_bytes()[res.len() - 2] != b'.';
                if pop {
                    if res.len() > 2 {
                        if last_segment_length == res.len() {
                            res.clear();
                            last_segment_length = 0;
                        } else {
                            let idx = res.len() - last_segment_length - 1;
                            res.truncate(idx);
                            last_segment_length = match res.rfind('\\') {
                                Some(pos) => res.len() - 1 - pos,
                                None => res.len(),
                            };
                        }
                        last_slash = i as isize;
                        dots = 0;
                        if i >= bytes.len() {
                            break;
                        }
                        i += 1;
                        continue;
                    } else if !res.is_empty() {
                        res.clear();
                        last_segment_length = 0;
                        last_slash = i as isize;
                        dots = 0;
                        if i >= bytes.len() {
                            break;
                        }
                        i += 1;
                        continue;
                    }
                }
                if allow_above_root {
                    if res.is_empty() {
                        res.push_str("..");
                    } else {
                        res.push_str("\\..");
                    }
                    last_segment_length = 2;
                }
            } else {
                let start = (last_slash + 1) as usize;
                let seg = &path[start..i];
                if res.is_empty() {
                    res.push_str(seg);
                } else {
                    res.push('\\');
                    res.push_str(seg);
                }
                last_segment_length = i - start;
            }
            last_slash = i as isize;
            dots = 0;
        } else if code == b'.' && dots != -1 {
            dots += 1;
        } else {
            dots = -1;
        }
        if i >= bytes.len() {
            break;
        }
        i += 1;
    }
    res
}

fn same_device(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// Resolve `home` against `cwd` the way `path.win32.resolve(cwd, home)` does
/// on Windows.
pub fn resolve(cwd: &str, home: &str) -> String {
    let mut resolved_device = String::new();
    let mut resolved_tail = String::new();
    let mut resolved_absolute = false;
    let mut step: i32 = 1;
    while step >= -1 {
        let fallback;
        let path: &str = if step >= 0 {
            let p = if step == 1 { home } else { cwd };
            if p.is_empty() {
                step -= 1;
                continue;
            }
            p
        } else if resolved_device.is_empty() {
            if cwd.is_empty() {
                break;
            }
            cwd
        } else {
            let on_device = cwd.len() >= 2 && same_device(&cwd[..2], &resolved_device);
            // A foreign drive (or a POSIX cwd, which names no drive) falls
            // back to that drive's root. That matches Node on Windows when
            // no per-drive cwd is set.
            fallback = if on_device { cwd.to_string() } else { format!("{resolved_device}\\") };
            fallback.as_str()
        };

        let bytes = path.as_bytes();
        let len = bytes.len();
        let mut root_end = 0usize;
        let mut device = String::new();
        let mut is_absolute = false;
        let code = bytes[0];

        if len == 1 {
            if is_sep(code) {
                root_end = 1;
                is_absolute = true;
            }
        } else if is_sep(code) {
            is_absolute = true;
            if is_sep(bytes[1]) {
                let mut j = 2usize;
                let mut last = j;
                while j < len && !is_sep(bytes[j]) {
                    j += 1;
                }
                if j < len && j != last {
                    let first = &path[last..j];
                    last = j;
                    while j < len && is_sep(bytes[j]) {
                        j += 1;
                    }
                    if j < len && j != last {
                        last = j;
                        while j < len && !is_sep(bytes[j]) {
                            j += 1;
                        }
                        if j == len || j != last {
                            device = format!("\\\\{first}\\{}", &path[last..j]);
                            root_end = j;
                        }
                    }
                }
            } else {
                root_end = 1;
            }
        } else if is_device(code) && len > 1 && bytes[1] == b':' {
            device = path[..2].to_string();
            root_end = 2;
            if len > 2 && is_sep(bytes[2]) {
                is_absolute = true;
                root_end = 3;
            }
        }

        if !device.is_empty() {
            if !resolved_device.is_empty() {
                if !same_device(&device, &resolved_device) {
                    step -= 1;
                    continue;
                }
            } else {
                resolved_device = device;
            }
        }

        if resolved_absolute {
            if !resolved_device.is_empty() {
                break;
            }
        } else {
            resolved_tail = format!("{}\\{resolved_tail}", &path[root_end..]);
            resolved_absolute = is_absolute;
            if is_absolute && !resolved_device.is_empty() {
                break;
            }
        }
        step -= 1;
    }

    resolved_tail = normalize_string(&resolved_tail, !resolved_absolute);
    if resolved_absolute {
        format!("{resolved_device}\\{resolved_tail}")
    } else if resolved_device.is_empty() && resolved_tail.is_empty() {
        ".".to_string()
    } else {
        format!("{resolved_device}{resolved_tail}")
    }
}

/// Lowercased absolute lexical home, trailing separator kept only for a
/// root (`D:\`, `\\server\share`).
pub fn pipe_key(home: &str, cwd: &str) -> String {
    let mut s = resolve(cwd, home);
    while s.len() > 3 && s.ends_with('\\') {
        s.pop();
    }
    // A UNC root `\\server\share` is longer than 3 and has no trailing
    // separator after resolve. A drive root is `D:\` (len 3).
    s.to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_win32_resolve_spellings() {
        let cases = [
            (r"C:\work", r"C:\Users\runneradmin\.pi\agent\pi-famulus", r"C:\Users\runneradmin\.pi\agent\pi-famulus"),
            (r"C:\work", "C:/Users/RunnerAdmin/.pi/agent/pi-famulus/", r"C:\Users\RunnerAdmin\.pi\agent\pi-famulus"),
            (r"C:\work", r"D:\", r"D:\"),
            (r"C:\work", r"D:/famulus\\", r"D:\famulus"),
            (r"C:\work\a", r".famulus", r"C:\work\a\.famulus"),
            (r"C:\work\b", r".famulus", r"C:\work\b\.famulus"),
            (r"C:\work\a", r"C:\work\a\proj\..\..\famulus", r"C:\work\famulus"),
            (r"C:\work\a", r"C:\work\a\proj\..\..\..\famulus", r"C:\famulus"),
            (r"C:\work", r"C:\work\\a\.\famulus\", r"C:\work\a\famulus"),
            (r"C:\work\proj", r"..\famulus", r"C:\work\famulus"),
            (r"C:\work", r"\abs", r"C:\abs"),
            (r"C:\work", "C:rel", r"C:\work\rel"),
            (r"C:\work", "D:rel", r"D:\rel"),
            (r"C:\work", r"\\server\share\a\..\b\", r"\\server\share\b"),
            (r"C:\work", r"C:\foo\bar\..\..\..\x", r"C:\x"),
            (r"C:\work", r"C:\Users\张三\.pi\agent\pi-famulus", r"C:\Users\张三\.pi\agent\pi-famulus"),
            (r"C:\work", r"\\?\C:\foo\..\bar", r"\\?\C:\bar"),
        ];
        for (cwd, home, want) in cases {
            assert_eq!(resolve(cwd, home), want, "cwd={cwd} home={home}");
        }
    }

    #[test]
    fn pipe_key_lowercases_and_keeps_drive_root() {
        assert_eq!(pipe_key(r".famulus", r"C:\work\a"), r"c:\work\a\.famulus");
        assert_eq!(pipe_key(r"D:\", r"C:\work"), r"d:\");
        assert_eq!(pipe_key(r"C:\work\a\.famulus\", r"D:\unused"), r"c:\work\a\.famulus");
        assert_ne!(pipe_key(r".famulus", r"C:\work\a"), pipe_key(r".famulus", r"C:\work\b"));
    }
}
