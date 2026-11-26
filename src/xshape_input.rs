//! X11 `ShapeInput` click-through worker (`--xshape-input`).
//!
//! Under XWayland/X11, Electron's `BrowserWindow.setShape(rects)` modifies X11
//! `ShapeBounding`, which clips *both* pointer input and visible pixels — slicing
//! off status-pill animations, tooltips, shadows, and toasts whenever they expand
//! outside the previous bounding box, and forcing Mutter to rebuild GPU mask
//! textures mid-animation.
//!
//! This lightweight stdin-driven worker keeps `ShapeBounding` completely unclipped
//! (`None`) so the 32-bit ARGB transparent canvas renders every animation frame at
//! full native refresh rate, while applying the interactive rectangles exclusively
//! to X11 `ShapeInput` (`SK::INPUT`) scaled accurately to physical X11 pixels.
//!
//! Protocol (one UTF-8 line per update on stdin):
//!   `[!]xid dip_w dip_h [x,y,w,h;x,y,w,h;...]`
//!
//! Leading `!` forces clearing `ShapeBounding` and refreshing cached X11 geometry
//! (used on window `show` / `resize`). An empty 4th field sets an empty input
//! region (100% click-through). Exits cleanly on stdin EOF.

use std::collections::HashMap;
use std::io::{self, BufRead};

use x11rb::connection::Connection;
use x11rb::protocol::shape::{self, SK, SO};
use x11rb::protocol::xproto::{ClipOrdering, ConnectionExt as _, Rectangle, Window};

#[derive(Debug, Clone, PartialEq)]
struct DipRect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

#[derive(Debug, Clone, PartialEq)]
struct ShapeCmd {
    force_reset: bool,
    xid: Window,
    dip_w: f64,
    dip_h: f64,
    rects: Vec<DipRect>,
}

fn parse_line(line: &str) -> Option<ShapeCmd> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut parts = trimmed.split_whitespace();
    let raw_xid = parts.next()?;
    let (force_reset, xid_str) = match raw_xid.strip_prefix('!') {
        Some(rest) => (true, rest),
        None => (false, raw_xid),
    };
    let xid: Window = if let Some(hex) = xid_str
        .strip_prefix("0x")
        .or_else(|| xid_str.strip_prefix("0X"))
    {
        u32::from_str_radix(hex, 16).ok()?
    } else {
        xid_str.parse().ok()?
    };
    if xid == 0 {
        return None;
    }
    let dip_w: f64 = parts.next()?.parse().ok()?;
    let dip_h: f64 = parts.next()?.parse().ok()?;
    let mut rects = Vec::new();
    if let Some(spec) = parts.next() {
        for item in spec.split(';') {
            if item.is_empty() {
                continue;
            }
            let mut nums = item.split(',');
            let x: f64 = nums.next()?.parse().ok()?;
            let y: f64 = nums.next()?.parse().ok()?;
            let w: f64 = nums.next()?.parse().ok()?;
            let h: f64 = nums.next()?.parse().ok()?;
            if x.is_finite()
                && y.is_finite()
                && w.is_finite()
                && h.is_finite()
                && w > 0.0
                && h > 0.0
            {
                rects.push(DipRect { x, y, w, h });
            }
        }
    }
    Some(ShapeCmd {
        force_reset,
        xid,
        dip_w,
        dip_h,
        rects,
    })
}

fn scale_rects(
    rects: &[DipRect],
    dip_w: f64,
    dip_h: f64,
    phys_w: u16,
    phys_h: u16,
) -> Vec<Rectangle> {
    let pw = f64::from(phys_w);
    let ph = f64::from(phys_h);
    let sx = if dip_w > 0.0 && pw > 0.0 {
        pw / dip_w
    } else {
        1.0
    };
    let sy = if dip_h > 0.0 && ph > 0.0 {
        ph / dip_h
    } else {
        1.0
    };

    let mut out = Vec::with_capacity(rects.len());
    for r in rects {
        let x0 = (r.x * sx).floor().clamp(0.0, pw);
        let y0 = (r.y * sy).floor().clamp(0.0, ph);
        let x1 = ((r.x + r.w) * sx).ceil().clamp(0.0, pw);
        let y1 = ((r.y + r.h) * sy).ceil().clamp(0.0, ph);
        if x1 > x0 && y1 > y0 {
            out.push(Rectangle {
                x: x0 as i16,
                y: y0 as i16,
                width: (x1 - x0) as u16,
                height: (y1 - y0) as u16,
            });
        }
    }
    out
}

pub fn run_xshape_input() {
    let (conn, _screen_num) = match x11rb::connect(None) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("xshape-input: X11 connect failed: {e}");
            std::process::exit(1);
        }
    };

    // Require XShape >= 1.1 (which introduces SK::INPUT).
    match shape::query_version(&conn) {
        Ok(cookie) => match cookie.reply() {
            Ok(ver) if (ver.major_version, ver.minor_version) >= (1, 1) => {}
            Ok(ver) => {
                eprintln!(
                    "xshape-input: XShape {}.{} lacks ShapeInput (need >= 1.1)",
                    ver.major_version, ver.minor_version
                );
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("xshape-input: XShape query_version reply failed: {e}");
                std::process::exit(1);
            }
        },
        Err(e) => {
            eprintln!("xshape-input: XShape query_version failed: {e}");
            std::process::exit(1);
        }
    }

    // Cache physical geometry per window alongside the last DIP size that matched it.
    let mut geom_cache: HashMap<Window, (u16, u16, i64, i64)> = HashMap::new();
    let stdin = io::stdin();

    for line_res in stdin.lock().lines() {
        let line = match line_res {
            Ok(l) => l,
            Err(_) => break,
        };
        let Some(cmd) = parse_line(&line) else {
            continue;
        };

        let dip_key = (cmd.dip_w.round() as i64, cmd.dip_h.round() as i64);
        let need_reset = cmd.force_reset || !geom_cache.contains_key(&cmd.xid);

        if need_reset {
            let _ = shape::mask(&conn, SO::SET, SK::BOUNDING, cmd.xid, 0, 0, x11rb::NONE);
        }

        let (phys_w, phys_h) = match geom_cache.get(&cmd.xid).copied() {
            Some((pw, ph, dw, dh)) if !need_reset && (dw, dh) == dip_key => (pw, ph),
            _ => match conn.get_geometry(cmd.xid).ok().and_then(|c| c.reply().ok()) {
                Some(g) => {
                    geom_cache.insert(cmd.xid, (g.width, g.height, dip_key.0, dip_key.1));
                    (g.width, g.height)
                }
                None => {
                    geom_cache.remove(&cmd.xid);
                    continue;
                }
            },
        };

        let xrects = scale_rects(&cmd.rects, cmd.dip_w, cmd.dip_h, phys_w, phys_h);
        let _ = shape::rectangles(
            &conn,
            SO::SET,
            SK::INPUT,
            ClipOrdering::UNSORTED,
            cmd.xid,
            0,
            0,
            &xrects,
        );
        if conn.flush().is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_force_and_rects() {
        let cmd = parse_line("!8388612 592 710 253,669,88,41;10,20,30,40").unwrap();
        assert!(cmd.force_reset);
        assert_eq!(cmd.xid, 8_388_612);
        assert_eq!(cmd.dip_w, 592.0);
        assert_eq!(cmd.dip_h, 710.0);
        assert_eq!(cmd.rects.len(), 2);
        assert_eq!(
            cmd.rects[0],
            DipRect {
                x: 253.0,
                y: 669.0,
                w: 88.0,
                h: 41.0
            }
        );
    }

    #[test]
    fn parses_empty_rects_as_clickthrough() {
        let cmd = parse_line("0x800004 592 710 ").unwrap();
        assert!(!cmd.force_reset);
        assert_eq!(cmd.xid, 0x800004);
        assert!(cmd.rects.is_empty());
    }

    #[test]
    fn scales_hidpi_and_clamps() {
        let rects = vec![DipRect {
            x: 253.0,
            y: 669.5,
            w: 88.0,
            h: 42.0,
        }];
        let scaled: Vec<(i16, i16, u16, u16)> = scale_rects(&rects, 592.0, 710.0, 1184, 1420)
            .into_iter()
            .map(|r| (r.x, r.y, r.width, r.height))
            .collect();
        assert_eq!(scaled, vec![(506, 1339, 176, 81)]);
    }
}
