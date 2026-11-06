//! Tiny plain-text persistence: which context each window belongs to, so
//! restarting Lanes keeps Work and Personal apart.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Saved {
    pub pid: i32,
    pub app: String,
    pub ctx: usize,
    pub frac: f64,
    pub order: usize,
    /// The screen this window shares and its area in it, if any.
    pub area: Option<(u32, [f64; 4])>,
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

fn state_path() -> PathBuf {
    home().join("Library/Application Support/Lanes/state.tsv")
}

pub fn contexts_path() -> PathBuf {
    home().join(".config/lanes/contexts.txt")
}

/// Context names: one per line in ~/.config/lanes/contexts.txt (max 9),
/// otherwise Work and Personal.
pub fn load_contexts() -> Vec<String> {
    let names: Vec<String> = fs::read_to_string(contexts_path())
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .take(9)
        .map(String::from)
        .collect();
    if names.is_empty() {
        vec!["Work".into(), "Personal".into()]
    } else {
        names
    }
}

pub fn load_state() -> (usize, HashMap<u32, Saved>) {
    let mut cur = 0;
    let mut map = HashMap::new();
    let Ok(text) = fs::read_to_string(state_path()) else {
        return (cur, map);
    };
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        match f.as_slice() {
            ["current", n] => cur = n.parse().unwrap_or(0),
            // Older files: no screens.
            ["w", id, pid, ctx, frac, _floating, order, app] => {
                if let Some(s) = parse(id, pid, ctx, frac, order, app, None) {
                    map.insert(s.0, s.1);
                }
            }
            ["w2", id, pid, ctx, frac, order, group, ax, ay, aw, ah, app] => {
                let area = match (
                    group.parse::<u32>(),
                    ax.parse::<f64>(),
                    ay.parse::<f64>(),
                    aw.parse::<f64>(),
                    ah.parse::<f64>(),
                ) {
                    (Ok(g), Ok(x), Ok(y), Ok(w), Ok(h)) if g > 0 => Some((g, [x, y, w, h])),
                    _ => None,
                };
                if let Some(s) = parse(id, pid, ctx, frac, order, app, area) {
                    map.insert(s.0, s.1);
                }
            }
            _ => {}
        }
    }
    (cur, map)
}

fn parse(
    id: &str,
    pid: &str,
    ctx: &str,
    frac: &str,
    order: &str,
    app: &str,
    area: Option<(u32, [f64; 4])>,
) -> Option<(u32, Saved)> {
    Some((
        id.parse().ok()?,
        Saved {
            pid: pid.parse().ok()?,
            app: app.to_string(),
            ctx: ctx.parse().ok()?,
            frac: frac.parse().ok()?,
            order: order.parse().ok()?,
            area,
        },
    ))
}

pub fn save_state(cur: usize, items: &[(u32, Saved)]) {
    let path = state_path();
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let mut out = String::with_capacity(64 + items.len() * 64);
    out.push_str(&format!("current\t{cur}\n"));
    for (id, s) in items {
        let app = s.app.replace(['\t', '\n'], " ");
        let (g, r) = s.area.unwrap_or((0, [0.0; 4]));
        out.push_str(&format!(
            "w2\t{id}\t{}\t{}\t{:.4}\t{}\t{g}\t{:.4}\t{:.4}\t{:.4}\t{:.4}\t{app}\n",
            s.pid, s.ctx, s.frac, s.order, r[0], r[1], r[2], r[3]
        ));
    }
    // Write atomically so a crash never leaves a half-written file.
    let tmp = path.with_extension("tmp");
    if let Ok(mut f) = fs::File::create(&tmp)
        && f.write_all(out.as_bytes()).is_ok()
    {
        let _ = fs::rename(&tmp, &path);
    }
}
