//! Launchpad: a full-screen, paged grid of the apps the user picked, with
//! type-to-filter, drag to rearrange, and folders (drop one app onto
//! another). Windows, surface and icons exist only while it is open.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, VK_ESCAPE, VK_LEFT, VK_RETURN, VK_RIGHT};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::app::{self, Cmd, Ctx};
use crate::config::{self, LaunchpadCfg};
use crate::render::anim::{lerp, Spring};
use crate::render::gfx::Surface;
use crate::util::{lp_x, lp_y, rect, rgba};
use crate::win::apps;
use crate::win::glass::Glass;
use crate::win::icons;

pub const ICON_CONSUMER: u8 = 1;
const COLS: usize = 7;
const ROWS: usize = 5;
const PER_PAGE: usize = COLS * ROWS;
const FOLDER_COLS: usize = 5;

struct Grid {
    left: f32,
    top: f32,
    cell_w: f32,
    cell_h: f32,
    icon: f32,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Entry {
    /// Index into `cx.apps`.
    App(usize),
    Folder(u32),
}

/// Left button is down on an icon; becomes a drag once the pointer moves.
struct Press {
    start: (f32, f32),
    /// Pointer offset from the icon's top-left corner.
    grab: (f32, f32),
    /// Index into `shown` (main grid) or into the open folder's apps.
    idx: usize,
    in_folder: bool,
}

struct Drag {
    /// Config key of the dragged item (launch string or `folder:<id>`).
    key: String,
    entry: Entry,
    /// Rearranging inside this open folder.
    in_folder: Option<u32>,
    pos: (f32, f32),
    grab: (f32, f32),
    /// Slot under the pointer and whether it is over the icon's centre,
    /// with the time it got there (moves wait for the pointer to settle).
    cand: Option<(usize, bool)>,
    since: Instant,
    /// Where the pointer was when it last stopped moving; rearranging waits
    /// for the pointer to rest, so passing over a gap on the way to an icon
    /// does not shuffle the grid.
    still: (f32, f32),
    /// Icon highlighted as a folder target.
    merge: Option<usize>,
    /// Pointer resting at a screen side to flip pages.
    edge: Option<(i32, Instant)>,
}

struct OpenFolder {
    id: u32,
    anim: Spring,
    closing: bool,
    renaming: bool,
}

pub struct Launchpad {
    glass: Glass,
    pub hwnd: HWND,
    surf: Option<Surface>,
    /// What the grid shows, in order.
    shown: Vec<Entry>,
    /// Config keys parallel to `shown` (arranging mode only).
    keys: Vec<String>,
    /// Folder id -> app indices / config keys of its apps.
    folder_apps: HashMap<u32, Vec<usize>>,
    folder_keys: HashMap<u32, Vec<String>>,
    icons: HashMap<usize, Option<ID2D1Bitmap>>,
    query: String,
    open: Spring,
    page: Spring,
    pub visible: bool,
    closing: bool,
    dirty: bool,
    /// Bumped whenever cached icon indices become invalid.
    generation: u32,
    /// Icons arrived; redrawn once the open animation has settled.
    icons_dirty: bool,
    /// Picking apps: every installed app is listed and a click toggles it.
    adding: bool,
    press: Option<Press>,
    drag: Option<Drag>,
    folder: Option<OpenFolder>,
    /// Animated positions (x includes the page offset) of grid and folder icons.
    pos: HashMap<Entry, (f32, f32)>,
    fpos: HashMap<usize, (f32, f32)>,
    moving: bool,
    /// Blurred wallpaper behind everything (made when Launchpad opens).
    backdrop: Option<ID2D1Bitmap>,
}

impl Launchpad {
    pub fn new(_cx: &Ctx) -> Self {
        let glass = Glass::new(0x70_14_10_10);
        let hwnd = app::create_window(WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOREDIRECTIONBITMAP, Some(glass.hwnd));
        Self {
            glass,
            hwnd,
            surf: None,
            shown: Vec::new(),
            keys: Vec::new(),
            folder_apps: HashMap::new(),
            folder_keys: HashMap::new(),
            icons: HashMap::new(),
            query: String::new(),
            open: Spring::new(0.0, 260.0, 30.0),
            page: Spring::new(0.0, 220.0, 28.0),
            visible: false,
            closing: false,
            dirty: false,
            generation: 0,
            icons_dirty: false,
            adding: false,
            press: None,
            drag: None,
            folder: None,
            pos: HashMap::new(),
            fpos: HashMap::new(),
            moving: false,
            backdrop: None,
        }
    }

    pub fn toggle(&mut self, cx: &mut Ctx) {
        if self.visible && !self.closing {
            self.close(cx);
        } else {
            self.show(cx);
        }
    }

    fn show(&mut self, cx: &mut Ctx) {
        cx.request_apps();
        self.query.clear();
        self.adding = false;
        self.folder = None;
        self.refilter(cx);
        self.page.snap(0.0);
        self.backdrop = crate::ui::backdrop::wallpaper(cx, cx.sw, cx.sh);
        if self.surf.is_none() {
            self.surf = Surface::new(&cx.gfx, self.hwnd, cx.sw as u32, cx.sh as u32).ok();
        }
        self.glass.set_rect(0, 0, cx.sw, cx.sh, 0);
        self.glass.set_opacity(0.0);
        self.glass.show(true);
        unsafe {
            let _ = SetWindowPos(self.hwnd, None, 0, 0, cx.sw, cx.sh, SWP_NOACTIVATE | SWP_NOZORDER);
            let _ = ShowWindow(self.hwnd, SW_SHOW);
            let _ = SetForegroundWindow(self.hwnd);
        }
        self.visible = true;
        self.closing = false;
        if !cx.animate {
            self.open.snap(1.0);
        }
        self.open.set(1.0);
        self.dirty = true;
    }

    pub fn close(&mut self, cx: &mut Ctx) {
        if !self.visible || self.closing {
            return;
        }
        self.end_drag(cx, false);
        self.finish_rename(cx);
        self.closing = true;
        self.open.set(0.0);
        if !cx.animate {
            self.open.snap(0.0);
        }
        self.dirty = true;
    }

    fn hide(&mut self) {
        self.visible = false;
        self.closing = false;
        self.press = None;
        self.drag = None;
        self.folder = None;
        self.glass.show(false);
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
        // Give the memory back: full-screen surface and all icon bitmaps.
        self.surf = None;
        self.backdrop = None;
        self.icons.clear();
        self.pos.clear();
        self.fpos.clear();
        self.generation = self.generation.wrapping_add(1);
    }

    /// The shared app list was replaced; cached indices are stale.
    pub fn apps_changed(&mut self, cx: &mut Ctx) {
        self.icons.clear();
        self.generation = self.generation.wrapping_add(1);
        if self.visible {
            self.end_drag(cx, false);
            self.refilter(cx);
        }
    }

    /// A background icon load finished.
    pub fn icon_ready(&mut self, generation: u32, index: usize, bmp: Option<ID2D1Bitmap>) {
        if generation == self.generation && self.visible {
            self.icons.insert(index, bmp);
            self.icons_dirty = true;
        }
    }

    /// Rearranging is possible: the user's own layout is shown, unfiltered.
    fn arranging(&self) -> bool {
        !self.adding && self.query.is_empty()
    }

    /// Rebuild `shown` from the config without moving the page.
    fn rebuild(&mut self, cx: &Ctx) {
        let find = |l: &str| cx.apps.iter().position(|a| a.launch.eq_ignore_ascii_case(l));
        let lp = &cx.cfg.launchpad;
        self.folder_apps.clear();
        self.folder_keys.clear();
        for f in &lp.folders {
            let pairs: Vec<(usize, String)> = f.apps.iter().filter_map(|k| find(k).map(|i| (i, k.clone()))).collect();
            self.folder_apps.insert(f.id, pairs.iter().map(|p| p.0).collect());
            self.folder_keys.insert(f.id, pairs.into_iter().map(|p| p.1).collect());
        }
        self.shown.clear();
        self.keys.clear();
        let q = self.query.to_lowercase();
        if self.adding || !q.is_empty() {
            let pool: Vec<usize> = if self.adding {
                (0..cx.apps.len()).collect()
            } else {
                // Search looks inside folders too.
                lp.apps.iter().chain(lp.folders.iter().flat_map(|f| f.apps.iter())).filter_map(|k| find(k)).collect()
            };
            let mut scored: Vec<(i32, usize)> = pool.into_iter().filter_map(|i| apps::score(&q, &cx.apps[i].lower).map(|s| (s, i))).collect();
            if !q.is_empty() {
                scored.sort_by(|a, b| b.0.cmp(&a.0));
            }
            scored.dedup_by_key(|p| p.1);
            for (_, i) in scored {
                self.shown.push(Entry::App(i));
                self.keys.push(cx.apps[i].launch.clone());
            }
        } else {
            for k in &lp.apps {
                let e = match LaunchpadCfg::folder_id(k) {
                    Some(id) => self.folder_apps.get(&id).filter(|a| !a.is_empty()).map(|_| Entry::Folder(id)),
                    None => find(k).map(Entry::App),
                };
                if let Some(e) = e {
                    self.shown.push(e);
                    self.keys.push(k.clone());
                }
            }
        }
        if self.page.target.round() as usize >= self.pages() {
            self.page.set(self.pages() as f32 - 1.0);
        }
        self.dirty = true;
    }

    fn refilter(&mut self, cx: &Ctx) {
        self.rebuild(cx);
        self.pos.clear();
        self.page.set(0.0);
    }

    fn pages(&self) -> usize {
        self.shown.len().div_ceil(PER_PAGE).max(1)
    }

    fn set_page(&mut self, page: i32) {
        let p = page.clamp(0, self.pages() as i32 - 1);
        self.page.set(p as f32);
        self.dirty = true;
    }

    fn grid(cx: &Ctx) -> Grid {
        let s = cx.s;
        let (w, h) = (cx.sw as f32, cx.sh as f32);
        let left = w * 0.11;
        let top = 120.0 * s;
        let cell_w = (w - left * 2.0) / COLS as f32;
        let cell_h = (h - top - 170.0 * s) / ROWS as f32;
        Grid { left, top, cell_w, cell_h, icon: (cell_h - 44.0 * s).min(84.0 * s) }
    }

    /// Icon rectangle (x, y) of grid slot `i` on its page at its resting position.
    fn slot(g: &Grid, i: usize, s: f32) -> (f32, f32) {
        let (c, r) = (i % COLS, (i % PER_PAGE) / COLS);
        let x = g.left + (c as f32 + 0.5) * g.cell_w - g.icon / 2.0;
        let y = g.top + r as f32 * g.cell_h + (g.cell_h - g.icon - 26.0 * s) / 2.0;
        (x, y)
    }

    /// Resting position of grid slot `i` with its page offset.
    fn slot_global(cx: &Ctx, g: &Grid, i: usize) -> (f32, f32) {
        let (x, y) = Self::slot(g, i, cx.s);
        (x + (i / PER_PAGE) as f32 * cx.sw as f32, y)
    }

    fn hit(&self, cx: &Ctx, x: f32, y: f32) -> Option<usize> {
        let g = Self::grid(cx);
        let page = self.page.target.round() as usize;
        let pad = 10.0 * cx.s;
        (page * PER_PAGE..((page + 1) * PER_PAGE).min(self.shown.len())).find(|&i| {
            let (ix, iy) = Self::slot(&g, i, cx.s);
            x >= ix - pad && x < ix + g.icon + pad && y >= iy - pad && y < iy + g.icon + 30.0 * cx.s
        })
    }

    /// Grid slot under a point while dragging (clamped to the last item)
    /// and whether the point is on that icon itself (not the gap around it).
    fn slot_at(&self, cx: &Ctx, x: f32, y: f32) -> Option<(usize, bool)> {
        if self.shown.is_empty() {
            return None;
        }
        let g = Self::grid(cx);
        let w = cx.sw as f32;
        if x < g.left || x >= w - g.left || y < g.top || y >= g.top + ROWS as f32 * g.cell_h {
            return None;
        }
        let col = (((x - g.left) / g.cell_w) as usize).min(COLS - 1);
        let row = (((y - g.top) / g.cell_h) as usize).min(ROWS - 1);
        let raw = self.page.target.round() as usize * PER_PAGE + row * COLS + col;
        let idx = raw.min(self.shown.len() - 1);
        let (ix, iy) = Self::slot(&g, idx, cx.s);
        let (cxp, cyp) = (ix + g.icon / 2.0, iy + g.icon / 2.0);
        // The whole icon is a folder target; only the space between icons rearranges.
        let reach = g.icon * 0.5;
        let on_icon = raw == idx && (x - cxp).abs() < reach && (y - cyp).abs() < reach;
        Some((idx, on_icon))
    }

    // ---- folders ---------------------------------------------------------

    fn folder_layout(&self, cx: &Ctx, n: usize) -> FolderLayout {
        let s = cx.s;
        let g = Self::grid(cx);
        let (w, h) = (cx.sw as f32, cx.sh as f32);
        let cols = n.clamp(3, FOLDER_COLS);
        let rows = n.div_ceil(cols).max(1);
        let cell_w = g.cell_w.min(150.0 * s);
        let cell_h = g.cell_h;
        let title_h = 64.0 * s;
        let pw = cols as f32 * cell_w + 48.0 * s;
        let ph = title_h + rows as f32 * cell_h + 24.0 * s;
        let px = (w - pw) / 2.0;
        let py = ((h - ph) / 2.0).max(90.0 * s);
        FolderLayout {
            panel: rect(px, py, pw, ph),
            title: rect(px, py + 18.0 * s, pw, 34.0 * s),
            left: px + 24.0 * s,
            top: py + title_h,
            cols,
            cell_w,
            cell_h,
            icon: g.icon,
            s,
        }
    }

    fn open_folder(&mut self, cx: &Ctx, id: u32) {
        self.fpos.clear();
        let mut anim = Spring::new(0.0, 320.0, 32.0);
        if !cx.animate {
            anim.snap(1.0);
        }
        anim.set(1.0);
        self.folder = Some(OpenFolder { id, anim, closing: false, renaming: false });
        self.dirty = true;
    }

    fn close_folder(&mut self, cx: &mut Ctx) {
        self.finish_rename(cx);
        if let Some(f) = &mut self.folder {
            f.closing = true;
            f.anim.set(0.0);
            if !cx.animate {
                self.folder = None;
            }
        }
        self.dirty = true;
    }

    fn folder_open_id(&self) -> Option<u32> {
        self.folder.as_ref().filter(|f| !f.closing).map(|f| f.id)
    }

    fn folder_name(cx: &Ctx, id: u32) -> String {
        cx.cfg.launchpad.folders.iter().find(|f| f.id == id).map(|f| f.name.clone()).unwrap_or_default()
    }

    fn finish_rename(&mut self, cx: &mut Ctx) {
        if let Some(f) = &mut self.folder {
            if f.renaming {
                f.renaming = false;
                if let Some(fc) = cx.cfg.launchpad.folder_mut(f.id) {
                    if fc.name.trim().is_empty() {
                        fc.name = "Folder".into();
                    }
                }
                cx.cfg.save();
                self.dirty = true;
            }
        }
    }

    // ---- adding / launching ------------------------------------------------

    /// Put an app in Launchpad or take it out (wherever it is, folders included).
    pub fn toggle_app(&mut self, cx: &mut Ctx, launch: &str) {
        let lp = &mut cx.cfg.launchpad;
        if lp.contains(launch) {
            lp.remove_app(launch);
        } else {
            lp.apps.push(launch.to_string());
        }
        cx.cfg.save();
        if self.folder_open_id().is_some_and(|id| !cx.cfg.launchpad.folders.iter().any(|f| f.id == id)) {
            self.folder = None;
        }
        self.rebuild(cx);
        self.dirty = true;
    }

    fn set_adding(&mut self, cx: &mut Ctx, on: bool) {
        self.folder = None;
        self.adding = on;
        self.query.clear();
        self.icons.clear();
        self.generation = self.generation.wrapping_add(1);
        self.refilter(cx);
    }

    /// The "Add Apps" / "Done" button, top right.
    fn mode_button(cx: &Ctx) -> D2D_RECT_F {
        let s = cx.s;
        rect(cx.sw as f32 / 2.0 + 150.0 * s, 52.0 * s, 110.0 * s, 30.0 * s)
    }

    fn launch(&mut self, cx: &mut Ctx, shown_idx: usize) {
        match self.shown.get(shown_idx).copied() {
            Some(Entry::App(ai)) if self.adding => {
                let launch = cx.apps[ai].launch.clone();
                self.toggle_app(cx, &launch);
            }
            Some(Entry::App(ai)) => {
                cx.cmds.push(Cmd::Open(cx.apps[ai].launch.clone()));
                self.close(cx);
            }
            Some(Entry::Folder(id)) => self.open_folder(cx, id),
            None => {}
        }
    }

    // ---- dragging ----------------------------------------------------------

    fn start_drag(&mut self, cx: &Ctx, p: Press, x: f32, y: f32) {
        let (key, entry, in_folder) = if p.in_folder {
            let Some(id) = self.folder_open_id() else { return };
            let (Some(k), Some(&ai)) = (self.folder_keys.get(&id).and_then(|v| v.get(p.idx)), self.folder_apps.get(&id).and_then(|v| v.get(p.idx))) else {
                return;
            };
            (k.clone(), Entry::App(ai), Some(id))
        } else {
            let (Some(k), Some(&e)) = (self.keys.get(p.idx), self.shown.get(p.idx)) else { return };
            (k.clone(), e, None)
        };
        let _ = cx;
        self.drag = Some(Drag { key, entry, in_folder, pos: (x, y), grab: p.grab, cand: None, since: Instant::now(), still: (x, y), merge: None, edge: None });
        unsafe {
            SetCapture(self.hwnd);
        }
        self.dirty = true;
    }

    /// Follow the pointer: rearrange, pick a folder target, flip pages,
    /// or pull an app out of the open folder.
    fn update_drag(&mut self, cx: &mut Ctx) {
        let Some(mut d) = self.drag.take() else { return };
        let (x, y) = d.pos;
        let now = Instant::now();
        if let Some(fid) = d.in_folder {
            let n = self.folder_apps.get(&fid).map_or(0, |v| v.len());
            let fl = self.folder_layout(cx, n);
            if !fl.contains(x, y) {
                // Out of the folder: the app goes back to the main grid, right after it.
                let lp = &mut cx.cfg.launchpad;
                if let Some(f) = lp.folder_mut(fid) {
                    f.apps.retain(|k| k != &d.key);
                }
                let fkey = LaunchpadCfg::folder_key(fid);
                let at = lp.apps.iter().position(|k| k == &fkey).map_or(lp.apps.len(), |i| i + 1);
                lp.apps.insert(at, d.key.clone());
                lp.tidy();
                self.folder = None;
                d.in_folder = None;
                d.cand = None;
                self.rebuild(cx);
                // Its new spot starts where the pointer is.
                let g = Self::grid(cx);
                self.pos.insert(d.entry, (x - d.grab.0 + self.page.v * cx.sw as f32, y - d.grab.1));
                let _ = g;
            } else if let Some(j) = fl.slot_at(x, y, n) {
                let keys = self.folder_keys.get(&fid).cloned().unwrap_or_default();
                let cur = keys.iter().position(|k| k == &d.key);
                if cur.is_some_and(|c| c != j) && d.cand == Some((j, false)) && now.duration_since(d.since) > Duration::from_millis(120) {
                    if let Some(f) = cx.cfg.launchpad.folder_mut(fid) {
                        config::move_to(&mut f.apps, &d.key, &keys[j]);
                    }
                    self.rebuild(cx);
                    self.moving = true;
                } else if d.cand != Some((j, false)) {
                    d.cand = Some((j, false));
                    d.since = now;
                }
            }
            self.drag = Some(d);
            self.dirty = true;
            return;
        }

        // Flip pages by resting at a screen side.
        let w = cx.sw as f32;
        let side = if x < 36.0 * cx.s { -1 } else if x > w - 36.0 * cx.s { 1 } else { 0 };
        match (side, d.edge) {
            (0, _) => d.edge = None,
            (s, Some((es, t))) if s == es => {
                if now.duration_since(t) > Duration::from_millis(550) {
                    let cur = self.page.target.round() as i32;
                    self.set_page(cur + s);
                    d.edge = Some((s, now));
                }
            }
            (s, _) => d.edge = Some((s, now)),
        }

        let me = self.keys.iter().position(|k| k == &d.key);
        // Judge by the centre of the icon being carried, not the pointer:
        // that is what the user lines up with the target.
        let icon = Self::grid(cx).icon;
        let (ccx, ccy) = (x - d.grab.0 + icon / 2.0, y - d.grab.1 + icon / 2.0);
        let cand = self.slot_at(cx, ccx, ccy).filter(|&(i, _)| Some(i) != me);
        if cand != d.cand {
            d.cand = cand;
            d.since = now;
            d.still = (x, y);
            d.merge = None;
        } else if (x - d.still.0).hypot(y - d.still.1) > 10.0 * cx.s {
            // Still moving: restart the wait.
            d.still = (x, y);
            if !d.cand.is_some_and(|c| c.1) {
                d.since = now;
            }
        }
        if let (Some((t, center)), Some(me)) = (d.cand, me) {
            let held = now.duration_since(d.since);
            let can_merge = matches!(d.entry, Entry::App(_));
            if center && can_merge {
                if held > Duration::from_millis(250) {
                    d.merge = Some(t);
                }
            } else if !center && held > Duration::from_millis(220) {
                let to = self.keys[t].clone();
                config::move_to(&mut cx.cfg.launchpad.apps, &d.key, &to);
                self.rebuild(cx);
                self.moving = true;
                d.cand = None;
                let _ = me;
            }
        }
        self.drag = Some(d);
        self.dirty = true;
    }

    /// Let go of the dragged icon (or cancel); saves the new layout.
    fn end_drag(&mut self, cx: &mut Ctx, drop: bool) {
        self.press = None;
        let Some(d) = self.drag.take() else { return };
        unsafe {
            let _ = ReleaseCapture();
        }
        if drop && d.in_folder.is_none() && matches!(d.entry, Entry::App(_)) {
            // Let go right on top of another icon: group them, even without waiting.
            let on_icon = d.cand.filter(|c| c.1).map(|c| c.0);
            if let Some(t) = d.merge.or(on_icon).filter(|&t| t < self.shown.len() && self.keys[t] != d.key) {
                let target = self.keys[t].clone();
                let lp = &mut cx.cfg.launchpad;
                match self.shown[t] {
                    Entry::Folder(id) => {
                        lp.apps.retain(|k| k != &d.key);
                        if let Some(f) = lp.folder_mut(id) {
                            f.apps.push(d.key.clone());
                        }
                    }
                    Entry::App(_) => {
                        lp.make_folder(&target, &d.key);
                    }
                }
                lp.tidy();
            }
        }
        cx.cfg.save();
        self.rebuild(cx);
        // Glide from where it was let go into its slot.
        let drop_at = (d.pos.0 - d.grab.0, d.pos.1 - d.grab.1);
        if d.in_folder.is_some() {
            if let Entry::App(ai) = d.entry {
                self.fpos.insert(ai, drop_at);
            }
        } else {
            self.pos.insert(d.entry, (drop_at.0 + self.page.v * cx.sw as f32, drop_at.1));
        }
        self.moving = true;
        self.dirty = true;
    }

    /// Ease every icon towards its slot. Returns true while anything moves.
    fn animate(&mut self, cx: &Ctx, dt: f32) {
        let k = 1.0 - (-dt * 16.0).exp();
        let g = Self::grid(cx);
        let mut moving = false;
        let dragged = self.drag.as_ref().map(|d| d.entry);
        let mut seen = std::collections::HashSet::new();
        for (i, &e) in self.shown.iter().enumerate() {
            seen.insert(e);
            let target = Self::slot_global(cx, &g, i);
            let p = self.pos.entry(e).or_insert(target);
            if Some(e) == dragged {
                continue;
            }
            p.0 += (target.0 - p.0) * k;
            p.1 += (target.1 - p.1) * k;
            if (target.0 - p.0).abs() > 0.4 || (target.1 - p.1).abs() > 0.4 {
                moving = true;
            } else {
                *p = target;
            }
        }
        self.pos.retain(|e, _| seen.contains(e));
        if let Some(fid) = self.folder.as_ref().map(|f| f.id) {
            let members = self.folder_apps.get(&fid).cloned().unwrap_or_default();
            let fl = self.folder_layout(cx, members.len());
            for (j, &ai) in members.iter().enumerate() {
                let target = fl.slot(j);
                let p = self.fpos.entry(ai).or_insert(target);
                if dragged == Some(Entry::App(ai)) && self.drag.as_ref().is_some_and(|d| d.in_folder.is_some()) {
                    continue;
                }
                p.0 += (target.0 - p.0) * k;
                p.1 += (target.1 - p.1) * k;
                if (target.0 - p.0).abs() > 0.4 || (target.1 - p.1).abs() > 0.4 {
                    moving = true;
                } else {
                    *p = target;
                }
            }
        }
        self.moving = moving;
    }

    // ---- input ---------------------------------------------------------------

    fn click(&mut self, cx: &mut Ctx, x: f32, y: f32) {
        // An open folder takes the click first.
        if let Some(fid) = self.folder_open_id() {
            let n = self.folder_apps.get(&fid).map_or(0, |v| v.len());
            let fl = self.folder_layout(cx, n);
            if fl.title_hit(x, y) {
                if let Some(f) = &mut self.folder {
                    f.renaming = true;
                }
                self.dirty = true;
                return;
            }
            self.finish_rename(cx);
            if let Some(j) = fl.hit(x, y, n) {
                if let Some(&ai) = self.folder_apps.get(&fid).and_then(|v| v.get(j)) {
                    cx.cmds.push(Cmd::Open(cx.apps[ai].launch.clone()));
                    self.close(cx);
                }
            } else if !fl.contains(x, y) {
                self.close_folder(cx);
            }
            return;
        }
        let b = Self::mode_button(cx);
        let empty_cta = !self.adding && cx.cfg.launchpad.apps.is_empty();
        let cta = rect(cx.sw as f32 / 2.0 - 70.0 * cx.s, cx.sh as f32 * 0.4 + 56.0 * cx.s, 140.0 * cx.s, 34.0 * cx.s);
        let inside = |r: &D2D_RECT_F| x >= r.left && x < r.right && y >= r.top && y < r.bottom;
        if inside(&b) || (empty_cta && inside(&cta)) {
            let on = !self.adding;
            self.set_adding(cx, on);
            return;
        }
        match self.hit(cx, x, y) {
            Some(i) => self.launch(cx, i),
            None => {
                // Page dots, otherwise a click on empty space dismisses.
                let s = cx.s;
                let pages = self.pages();
                let dots_y = cx.sh as f32 - 128.0 * s;
                let x0 = cx.sw as f32 / 2.0 - (pages as f32 - 1.0) * 9.0 * s;
                let dot = (0..pages).find(|&p| (x - (x0 + p as f32 * 18.0 * s)).abs() < 9.0 * s && (y - dots_y).abs() < 12.0 * s);
                match dot {
                    Some(p) => self.set_page(p as i32),
                    None => self.close(cx),
                }
            }
        }
    }

    pub fn on_msg(&mut self, cx: &mut Ctx, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        if hwnd != self.hwnd {
            return None;
        }
        match msg {
            WM_LBUTTONDOWN => {
                let (x, y) = (lp_x(lp) as f32, lp_y(lp) as f32);
                self.press = None;
                if self.arranging() {
                    if let Some(fid) = self.folder_open_id() {
                        let n = self.folder_apps.get(&fid).map_or(0, |v| v.len());
                        let fl = self.folder_layout(cx, n);
                        if let Some(j) = fl.hit(x, y, n) {
                            let (ix, iy) = fl.slot(j);
                            self.press = Some(Press { start: (x, y), grab: (x - ix, y - iy), idx: j, in_folder: true });
                        }
                    } else if let Some(i) = self.hit(cx, x, y) {
                        let (ix, iy) = Self::slot(&Self::grid(cx), i, cx.s);
                        self.press = Some(Press { start: (x, y), grab: (x - ix, y - iy), idx: i, in_folder: false });
                    }
                }
                Some(LRESULT(0))
            }
            WM_MOUSEMOVE => {
                let (x, y) = (lp_x(lp) as f32, lp_y(lp) as f32);
                if let Some(d) = &mut self.drag {
                    d.pos = (x, y);
                    self.update_drag(cx);
                } else if let Some(p) = &self.press {
                    if (x - p.start.0).hypot(y - p.start.1) > 6.0 * cx.s {
                        let p = self.press.take().unwrap();
                        self.finish_rename(cx);
                        self.start_drag(cx, p, x, y);
                    }
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONUP => {
                let (x, y) = (lp_x(lp) as f32, lp_y(lp) as f32);
                if self.drag.is_some() {
                    self.end_drag(cx, true);
                } else {
                    self.press = None;
                    self.click(cx, x, y);
                }
                Some(LRESULT(0))
            }
            WM_CAPTURECHANGED => {
                if self.drag.is_some() {
                    self.end_drag(cx, true);
                }
                Some(LRESULT(0))
            }
            WM_RBUTTONUP => {
                let (x, y) = (lp_x(lp), lp_y(lp));
                if self.adding || self.drag.is_some() {
                    return Some(LRESULT(0));
                }
                let ai = match self.folder_open_id() {
                    Some(fid) => {
                        let n = self.folder_apps.get(&fid).map_or(0, |v| v.len());
                        self.folder_layout(cx, n).hit(x as f32, y as f32, n).and_then(|j| self.folder_apps.get(&fid)?.get(j).copied())
                    }
                    None => match self.hit(cx, x as f32, y as f32).and_then(|i| self.shown.get(i)) {
                        Some(&Entry::App(ai)) => Some(ai),
                        _ => None,
                    },
                };
                if let Some(ai) = ai {
                    cx.cmds.push(Cmd::AppMenu(cx.apps[ai].launch.clone(), x, y));
                }
                Some(LRESULT(0))
            }
            WM_MOUSEWHEEL => {
                if self.folder.is_none() {
                    let delta = (wp.0 >> 16) as i16;
                    let cur = self.page.target.round() as i32;
                    self.set_page(if delta < 0 { cur + 1 } else { cur - 1 });
                }
                Some(LRESULT(0))
            }
            WM_KEYDOWN => {
                let cur = self.page.target.round() as i32;
                let renaming = self.folder.as_ref().is_some_and(|f| f.renaming);
                match wp.0 as u16 {
                    k if renaming && (k == VK_ESCAPE.0 || k == VK_RETURN.0) => self.finish_rename(cx),
                    k if k == VK_ESCAPE.0 => {
                        if self.drag.is_some() {
                            self.end_drag(cx, false);
                        } else if self.folder_open_id().is_some() {
                            self.close_folder(cx);
                        } else if self.adding && self.query.is_empty() {
                            self.set_adding(cx, false);
                        } else if self.query.is_empty() {
                            self.close(cx);
                        } else {
                            self.query.clear();
                            self.refilter(cx);
                        }
                    }
                    k if k == VK_LEFT.0 && self.folder.is_none() => self.set_page(cur - 1),
                    k if k == VK_RIGHT.0 && self.folder.is_none() => self.set_page(cur + 1),
                    k if k == VK_RETURN.0 => {
                        if !self.query.is_empty() && !self.shown.is_empty() {
                            self.launch(cx, 0);
                        }
                    }
                    _ => {}
                }
                Some(LRESULT(0))
            }
            WM_CHAR => {
                let c = char::from_u32(wp.0 as u32);
                if let Some(id) = self.folder.as_ref().filter(|f| f.renaming).map(|f| f.id) {
                    if let Some(fc) = cx.cfg.launchpad.folder_mut(id) {
                        match c {
                            Some('\u{8}') => {
                                fc.name.pop();
                            }
                            Some(c) if !c.is_control() && fc.name.chars().count() < 40 => fc.name.push(c),
                            _ => {}
                        }
                    }
                    self.dirty = true;
                    return Some(LRESULT(0));
                }
                if self.drag.is_some() {
                    return Some(LRESULT(0));
                }
                match c {
                    Some('\u{8}') => {
                        self.query.pop();
                        self.refilter(cx);
                    }
                    Some(c) if !c.is_control() => {
                        // Typing searches everything; an open folder makes way.
                        self.folder = None;
                        self.query.push(c);
                        self.refilter(cx);
                    }
                    _ => {}
                }
                Some(LRESULT(0))
            }
            WM_ACTIVATE => {
                if (wp.0 & 0xFFFF) as u32 == WA_INACTIVE {
                    self.close(cx);
                }
                Some(LRESULT(0))
            }
            _ => None,
        }
    }

    pub fn needs_frame(&self) -> bool {
        self.visible
            && (self.dirty
                || self.icons_dirty
                || self.open.active()
                || self.page.active()
                || self.drag.is_some()
                || self.moving
                || self.folder.as_ref().is_some_and(|f| f.anim.active()))
    }

    // ---- drawing ---------------------------------------------------------------

    fn request_icon(&mut self, cx: &Ctx, ai: usize, px: i32) {
        if !self.icons.contains_key(&ai) {
            self.icons.insert(ai, None);
            cx.icon_loader.request(icons::icon_id(ICON_CONSUMER, self.generation, ai), &cx.apps[ai].launch, px);
        }
    }

    fn draw_app_icon(&self, cx: &mut Ctx, ai: usize, r: D2D_RECT_F, a: f32) {
        match self.icons.get(&ai) {
            Some(Some(bmp)) => cx.gfx.bitmap(bmp, r, a),
            _ => {
                let size = r.right - r.left;
                cx.gfx.fill_rrect(r, size * 0.22, rgba(1.0, 1.0, 1.0, 0.08 * a));
            }
        }
    }

    /// A folder tile: frosted square with up to nine tiny icons.
    fn draw_folder_icon(&self, cx: &mut Ctx, id: u32, r: D2D_RECT_F, a: f32) {
        let size = r.right - r.left;
        cx.gfx.fill_rrect(r, size * 0.24, rgba(1.0, 1.0, 1.0, 0.22 * a));
        cx.gfx.stroke_rrect(r, size * 0.24, 1.0, rgba(1.0, 1.0, 1.0, 0.18 * a));
        let pad = size * 0.13;
        let cell = (size - pad * 2.0) / 3.0;
        let mini = cell * 0.82;
        if let Some(members) = self.folder_apps.get(&id) {
            for (j, &ai) in members.iter().take(9).enumerate() {
                let x = r.left + pad + (j % 3) as f32 * cell + (cell - mini) / 2.0;
                let y = r.top + pad + (j / 3) as f32 * cell + (cell - mini) / 2.0;
                self.draw_app_icon(cx, ai, rect(x, y, mini, mini), a);
            }
        }
    }

    fn draw_entry(&self, cx: &mut Ctx, e: Entry, r: D2D_RECT_F, a: f32) {
        match e {
            Entry::App(ai) => self.draw_app_icon(cx, ai, r, a),
            Entry::Folder(id) => self.draw_folder_icon(cx, id, r, a),
        }
    }

    fn entry_name(&self, cx: &Ctx, e: Entry) -> String {
        match e {
            Entry::App(ai) => cx.apps[ai].name.clone(),
            Entry::Folder(id) => Self::folder_name(cx, id),
        }
    }

    pub fn frame(&mut self, cx: &mut Ctx, dt: f32) {
        if !self.visible {
            return;
        }
        // Content changes, page slides, drags and folder animations need a
        // redraw; the zoom and fade of opening/closing are done by the compositor.
        let folder_anim = self.folder.as_ref().is_some_and(|f| f.anim.active());
        let redraw = self.dirty || self.page.active() || self.drag.is_some() || self.moving || folder_anim || (self.icons_dirty && !self.open.active());
        if redraw {
            self.icons_dirty = false;
        }
        self.dirty = false;
        self.open.tick(dt);
        self.page.tick(dt);
        if let Some(f) = &mut self.folder {
            f.anim.tick(dt);
            if f.closing && f.anim.v < 0.02 {
                self.folder = None;
            }
        }
        if self.closing && self.open.v < 0.02 {
            self.hide();
            return;
        }
        // Waiting on a still pointer (folder target, page flip) needs no mouse move.
        if self.drag.is_some() {
            self.update_drag(cx);
            self.dirty = false;
        }
        self.animate(cx, dt);
        let t = self.open.v.clamp(0.0, 1.0);
        self.glass.set_opacity(t);
        let (w, h) = (cx.sw as f32, cx.sh as f32);
        if let Some(surf) = &mut self.surf {
            surf.set_transform(&cx.gfx, lerp(1.14, 1.0, t), w / 2.0, h / 2.0, t);
        }
        if !redraw {
            return;
        }
        let s = cx.s;
        let g = Self::grid(cx);
        let icon_px = g.icon.ceil() as i32;

        // Ask the worker thread for icons of the pages in view; they pop in as they arrive.
        let first_page = (self.page.v.floor().max(0.0)) as usize;
        let range = first_page * PER_PAGE..((first_page + 2) * PER_PAGE).min(self.shown.len());
        for i in range.clone() {
            match self.shown[i] {
                Entry::App(ai) => self.request_icon(cx, ai, icon_px),
                Entry::Folder(id) => {
                    let members: Vec<usize> = self.folder_apps.get(&id).map(|v| v.iter().take(9).copied().collect()).unwrap_or_default();
                    for ai in members {
                        self.request_icon(cx, ai, icon_px);
                    }
                }
            }
        }
        if let Some(fid) = self.folder.as_ref().map(|f| f.id) {
            for ai in self.folder_apps.get(&fid).cloned().unwrap_or_default() {
                self.request_icon(cx, ai, icon_px);
            }
        }

        let Some(surf) = &self.surf else { return };
        surf.begin(&cx.gfx);
        let fo = self.folder.as_ref().map_or(0.0, |f| f.anim.v.clamp(0.0, 1.0));
        // The grid fades back while a folder is open.
        let base = 1.0 - 0.8 * fo;

        // Blurred wallpaper, darkened a little so labels stay readable (like macOS).
        if let Some(bg) = &self.backdrop {
            cx.gfx.bitmap(bg, rect(0.0, 0.0, w, h), 1.0);
            cx.gfx.fill_rect(rect(0.0, 0.0, w, h), rgba(0.0, 0.0, 0.0, 0.18 + 0.12 * fo));
        }

        // Search field.
        let (fw, fh) = (260.0 * s, 30.0 * s);
        let field = rect(w / 2.0 - fw / 2.0, 52.0 * s, fw, fh);
        cx.gfx.fill_rrect(field, 8.0 * s, rgba(1.0, 1.0, 1.0, 0.13 * base));
        cx.gfx.stroke_rrect(field, 8.0 * s, 1.0, rgba(1.0, 1.0, 1.0, 0.22 * base));
        cx.gfx.glyph('\u{E721}', 13.0 * s, rect(field.left + 6.0 * s, field.top, 22.0 * s, fh), rgba(1.0, 1.0, 1.0, 0.6 * base));
        let text_r = rect(field.left + 30.0 * s, field.top, fw - 38.0 * s, fh);
        if self.query.is_empty() {
            cx.gfx.text("Search", 13.5 * s, 400, text_r, -1, rgba(1.0, 1.0, 1.0, 0.45 * base));
        } else {
            cx.gfx.text(&self.query, 13.5 * s, 400, text_r, -1, rgba(1.0, 1.0, 1.0, 0.95 * base));
        }

        let dragged = self.drag.as_ref().map(|d| d.entry);
        let merge = self.drag.as_ref().and_then(|d| d.merge);

        // App grid; neighbouring pages slide in from the side.
        for i in range {
            let e = self.shown[i];
            if Some(e) == dragged && self.drag.as_ref().is_some_and(|d| d.in_folder.is_none()) {
                continue;
            }
            let (gx, gy) = self.pos.get(&e).copied().unwrap_or_else(|| Self::slot_global(cx, &g, i));
            let x = gx - self.page.v * w;
            let y = gy;
            if x <= -g.cell_w || x >= w {
                continue;
            }
            let off = ((i / PER_PAGE) as f32 - self.page.v) * w;
            let a = base * (1.0 - (off.abs() / w)).clamp(0.0, 1.0);
            let still = !self.moving && !self.page.active();
            if merge == Some(i) {
                let grow = g.icon * 0.12;
                let hr = rect(x - grow, y - grow, g.icon + grow * 2.0, g.icon + grow * 2.0);
                cx.gfx.fill_rrect(hr, (g.icon + grow * 2.0) * 0.24, rgba(1.0, 1.0, 1.0, 0.28 * a));
            }
            // Whole pixels when everything is still, for crisp icons.
            let r = if still { rect(x.round(), y.round(), g.icon.round(), g.icon.round()) } else { rect(x, y, g.icon, g.icon) };
            self.draw_entry(cx, e, r, a);
            let label = rect(x + g.icon / 2.0 - g.cell_w / 2.0 + 4.0 * s, y + g.icon + 6.0 * s, g.cell_w - 8.0 * s, 22.0 * s);
            let name = self.entry_name(cx, e);
            cx.gfx.text(&name, 13.0 * s, 400, label, 0, rgba(1.0, 1.0, 1.0, 0.95 * a));
            if let (true, Entry::App(ai)) = (self.adding, e) {
                // Badge: blue check when the app is in Launchpad, grey plus when not.
                let on = cx.cfg.launchpad.contains(&cx.apps[ai].launch);
                let (bx, by, br) = (x + g.icon - 4.0 * s, y + 4.0 * s, 11.0 * s);
                let bg = if on { rgba(0.04, 0.52, 1.0, a) } else { rgba(0.35, 0.35, 0.38, 0.95 * a) };
                cx.gfx.fill_circle(bx, by, br, bg);
                cx.gfx.glyph(if on { '\u{E73E}' } else { '\u{E710}' }, 10.0 * s, rect(bx - br, by - br, br * 2.0, br * 2.0), rgba(1.0, 1.0, 1.0, a));
            }
        }
        if self.shown.is_empty() {
            if !self.adding && cx.cfg.launchpad.apps.is_empty() && self.query.is_empty() {
                cx.gfx.text("Launchpad is empty", 24.0 * s, 300, rect(0.0, h * 0.4 - 10.0 * s, w, 40.0 * s), 0, rgba(1.0, 1.0, 1.0, 0.85));
                cx.gfx.text("Choose the apps you want to see here.", 14.0 * s, 400, rect(0.0, h * 0.4 + 26.0 * s, w, 24.0 * s), 0, rgba(1.0, 1.0, 1.0, 0.55));
                let cta = rect(w / 2.0 - 70.0 * s, h * 0.4 + 56.0 * s, 140.0 * s, 34.0 * s);
                cx.gfx.fill_rrect(cta, 8.0 * s, rgba(0.04, 0.52, 1.0, 1.0));
                cx.gfx.text("Add Apps", 14.0 * s, 600, cta, 0, rgba(1.0, 1.0, 1.0, 1.0));
            } else {
                cx.gfx.text("No Results", 22.0 * s, 300, rect(0.0, h * 0.4, w, 40.0 * s), 0, rgba(1.0, 1.0, 1.0, 0.6));
            }
        }
        // Mode button next to the search field.
        let b = Self::mode_button(cx);
        let (label, bg) = if self.adding { ("Done", rgba(0.04, 0.52, 1.0, base)) } else { ("Add Apps", rgba(1.0, 1.0, 1.0, 0.13 * base)) };
        cx.gfx.fill_rrect(b, 8.0 * s, bg);
        cx.gfx.text(label, 13.0 * s, 600, b, 0, rgba(1.0, 1.0, 1.0, 0.95 * base));

        // Page dots.
        let pages = self.pages();
        if pages > 1 {
            let x0 = w / 2.0 - (pages as f32 - 1.0) * 9.0 * s;
            for p in 0..pages {
                let near = 1.0 - (p as f32 - self.page.v).abs().min(1.0);
                cx.gfx.fill_circle(x0 + p as f32 * 18.0 * s, h - 128.0 * s, 3.5 * s, rgba(1.0, 1.0, 1.0, (0.35 + 0.55 * near) * base));
            }
        }

        // Open folder: a frosted panel with its apps, zooming out of the grid.
        if let Some(fid) = self.folder.as_ref().map(|f| f.id) {
            let members = self.folder_apps.get(&fid).cloned().unwrap_or_default();
            let fl = self.folder_layout(cx, members.len());
            let renaming = self.folder.as_ref().is_some_and(|f| f.renaming);
            cx.gfx.set_scale(lerp(0.82, 1.0, fo), w / 2.0, h / 2.0);
            let pr = fl.panel;
            cx.gfx.fill_rrect(pr, 26.0 * s, rgba(0.16, 0.16, 0.18, 0.62 * fo));
            cx.gfx.stroke_rrect(pr, 26.0 * s, 1.0, rgba(1.0, 1.0, 1.0, 0.16 * fo));
            let mut name = Self::folder_name(cx, fid);
            if renaming {
                let tr = fl.title;
                let tw = (cx.gfx.text_width(&name, 22.0 * s, 600) + 30.0 * s).max(120.0 * s);
                let field = rect((tr.left + tr.right) / 2.0 - tw / 2.0, tr.top, tw, tr.bottom - tr.top);
                cx.gfx.fill_rrect(field, 7.0 * s, rgba(1.0, 1.0, 1.0, 0.14 * fo));
                cx.gfx.stroke_rrect(field, 7.0 * s, 1.5, rgba(0.04, 0.52, 1.0, fo));
                name.push('|');
            }
            cx.gfx.text(&name, 22.0 * s, 600, fl.title, 0, rgba(1.0, 1.0, 1.0, 0.95 * fo));
            let dragging_inside = self.drag.as_ref().filter(|d| d.in_folder.is_some()).map(|d| d.entry);
            for (j, &ai) in members.iter().enumerate() {
                if dragging_inside == Some(Entry::App(ai)) {
                    continue;
                }
                let (x, y) = self.fpos.get(&ai).copied().unwrap_or_else(|| fl.slot(j));
                let r = if self.moving || folder_anim { rect(x, y, fl.icon, fl.icon) } else { rect(x.round(), y.round(), fl.icon.round(), fl.icon.round()) };
                self.draw_app_icon(cx, ai, r, fo);
                let label = rect(x + fl.icon / 2.0 - fl.cell_w / 2.0 + 4.0 * s, y + fl.icon + 6.0 * s, fl.cell_w - 8.0 * s, 22.0 * s);
                cx.gfx.text(&cx.apps[ai].name, 13.0 * s, 400, label, 0, rgba(1.0, 1.0, 1.0, 0.95 * fo));
            }
            cx.gfx.reset_transform();
        }

        // The icon being dragged floats above everything, slightly enlarged.
        if let Some(d) = &self.drag {
            let size = g.icon * 1.08;
            let (x, y) = (d.pos.0 - d.grab.0 - (size - g.icon) / 2.0, d.pos.1 - d.grab.1 - (size - g.icon) / 2.0);
            self.draw_entry(cx, d.entry, rect(x, y, size, size), 0.92);
        }
        surf.end(&cx.gfx);
    }
}

/// Geometry of the open folder panel.
struct FolderLayout {
    panel: D2D_RECT_F,
    title: D2D_RECT_F,
    left: f32,
    top: f32,
    cols: usize,
    cell_w: f32,
    cell_h: f32,
    icon: f32,
    s: f32,
}

impl FolderLayout {
    fn slot(&self, j: usize) -> (f32, f32) {
        let (c, r) = (j % self.cols, j / self.cols);
        let x = self.left + (c as f32 + 0.5) * self.cell_w - self.icon / 2.0;
        let y = self.top + r as f32 * self.cell_h + (self.cell_h - self.icon - 26.0 * self.s) / 2.0;
        (x, y)
    }

    fn contains(&self, x: f32, y: f32) -> bool {
        let m = 24.0 * self.s;
        x >= self.panel.left - m && x < self.panel.right + m && y >= self.panel.top - m && y < self.panel.bottom + m
    }

    fn title_hit(&self, x: f32, y: f32) -> bool {
        x >= self.title.left && x < self.title.right && y >= self.title.top && y < self.title.bottom
    }

    fn hit(&self, x: f32, y: f32, n: usize) -> Option<usize> {
        let pad = 10.0 * self.s;
        (0..n).find(|&j| {
            let (ix, iy) = self.slot(j);
            x >= ix - pad && x < ix + self.icon + pad && y >= iy - pad && y < iy + self.icon + 30.0 * self.s
        })
    }

    /// Slot under the pointer while rearranging (clamped to the last app).
    fn slot_at(&self, x: f32, y: f32, n: usize) -> Option<usize> {
        if n == 0 {
            return None;
        }
        let col = ((x - self.left) / self.cell_w).floor().clamp(0.0, self.cols as f32 - 1.0) as usize;
        let row = ((y - self.top) / self.cell_h).floor().max(0.0) as usize;
        Some((row * self.cols + col).min(n - 1))
    }
}
