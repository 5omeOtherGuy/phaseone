// p1-cells — the reference cell renderer for the p1 TUI (SLAB Harness).
// Ports SLAB Harness Band 1:1 to a cell grid, implements the geometry decision table of
// handoff/TUI-HANDOFF.md §4, composes screens from component recipes (window.P1.*), and
// serialises grids as TEXT + RUNS for the handoff. Pure: no DOM needed except GridView.
(function () {
  var g = typeof window !== "undefined" ? window : globalThis;
  var TONE = { ink: "i", fg: "i", dim: "d", faint: "f", attn: "a", fail: "x", ok: "o", live: "l", ref: "r", syntax: "s", ground: "g", rule: "u" };
  var VAR = { "var(--h-ink)": "i", "var(--h-dim)": "d", "var(--h-faint)": "f", "var(--slab-attn)": "a", "var(--slab-fail)": "x", "var(--slab-ok)": "o", "var(--slab-live)": "l", "var(--slab-ref)": "r", "var(--slab-syntax)": "s", "var(--h-ground)": "g", "var(--h-rule)": "u", "var(--slab-diff-add-fg)": "+", "var(--slab-diff-del-fg)": "-", "var(--h-route-chip-bg)": "i" };
  var BG = { "var(--h-ground)": "G", "var(--h-block)": "B", "var(--h-block-plus)": "P", "var(--slab-diff-add-bg)": "+", "var(--slab-diff-del-bg)": "-", "var(--h-focus-row-bg)": "A", "var(--h-decision-key-bg)": "A" };
  var INV = { a: "A", i: "N" };
  var FG_CSS = { i: "var(--h-ink)", d: "var(--h-dim)", f: "var(--h-faint)", a: "var(--slab-attn)", x: "var(--slab-fail)", o: "var(--slab-ok)", l: "var(--slab-live)", r: "var(--slab-ref)", s: "var(--slab-syntax)", g: "var(--h-ground)", u: "var(--h-rule)", "+": "var(--slab-diff-add-fg)", "-": "var(--slab-diff-del-fg)", _: "var(--h-ink)" };
  var BG_CSS = { G: "var(--h-ground)", B: "var(--h-block)", P: "var(--h-block-plus)", "+": "var(--slab-diff-add-bg)", "-": "var(--slab-diff-del-bg)", A: "var(--slab-attn)", N: "var(--h-ink)" };
  function chars(s) { return Array.from(String(s == null ? "" : s)); }
  function fgc(t) { return TONE[t] || VAR[t] || "i"; }
  function bgc(b) { if (!b || b === "transparent") return null; return BG[b] || null; }
  function segLen(segs) { return segs.reduce(function (n, s) { return n + chars(s[1]).length; }, 0); }
  function cell(ch, fg, bg) { return { ch: ch, fg: ch === " " ? "_" : fg, bg: bg }; }

  // SLAB Harness Band, cell for cell.
  function band(r, base) {
    var W = r.width || 76, pad = r.pad == null ? 2 : r.pad, rowBg = bgc(r.bg) || base || "G";
    var U = W - pad * 2, L = (r.left || []).map(function (s) { return [s[0], String(s[1] == null ? "" : s[1]), s[2]]; }), R = r.right || [];
    var over = segLen(L) + segLen(R) + (R.length ? (r.minGap == null ? 2 : r.minGap) : 0) - U;
    for (var i = L.length - 1; over > 0 && i >= 0; i--) {
      var t = chars(L[i][1]);
      if (t.length > over + 1) { L[i][1] = t.slice(0, t.length - over - 1).join("") + "\u2026"; over = 0; }
      else { over -= t.length; L[i][1] = ""; }
    }
    var gap = Math.max(0, U - segLen(L) - segLen(R)), out = [];
    function spaces(n) { for (var k = 0; k < n; k++) out.push(cell(" ", "_", rowBg)); }
    function seg(s) {
      var fg = fgc(s[0]), o = s[2] || {}, bg = bgc(o.bg) || rowBg;
      if (o.inverse) { bg = INV[fg] || "N"; fg = "g"; }
      chars(s[1]).forEach(function (c) { out.push(cell(c, fg, bg)); });
    }
    spaces(pad); L.forEach(seg); spaces(gap); R.forEach(seg); spaces(pad);
    out = out.slice(0, W); while (out.length < W) out.push(cell(" ", "_", rowBg));
    if (r.working) { var at = r.working.left != null ? r.working.left : W - r.working.right - 3; for (var k = 0; k < 3; k++) if (out[at + k]) out[at + k] = { ch: "\u25aa", fg: "l", bg: out[at + k].bg, work: k }; }
    return out;
  }
  function cells(rows, base) { return (rows || []).map(function (r) { return r.cells || band(r, base); }); }

  function make(W, H) { var rows = []; for (var y = 0; y < H; y++) { var r = []; for (var x = 0; x < W; x++) r.push(cell(" ", "_", "G")); rows.push(r); } return { W: W, H: H, rows: rows }; }
  function blit(grid, x, y, row) { if (y < 0 || y >= grid.H) return; row.forEach(function (c, i) { if (x + i >= 0 && x + i < grid.W) grid.rows[y][x + i] = c; }); }
  function fill(grid, x, y, w, h, bg) { for (var j = 0; j < h; j++) for (var i = 0; i < w; i++) if (y + j < grid.H && x + i < grid.W) grid.rows[y + j][x + i] = cell(" ", "_", bg); }
  function blitRows(grid, x, y, list, max) { list.slice(0, max == null ? list.length : max).forEach(function (r, i) { blit(grid, x, y + i, r); }); }

  // §4 geometry decision table.
  function geometry(W, H, o) {
    o = o || {};
    var focus = o.focus != null ? o.focus : H <= 12, P = 0;
    if (!focus && !o.full && W >= 100 && o.paneWidth !== "off") {
      var want = o.paneWidth == null || o.paneWidth === "auto" ? (W >= 160 ? 56 : 38) : o.paneWidth === "split" ? Math.floor((W - 6) / 2) : o.paneWidth;
      if (W - 6 - want < 56) want = 38;
      if (W - 6 - want < 56) want = 0;
      P = want;
    }
    var T = P ? W - 6 - P : Math.min(W - 4, 120);
    if (P && T > 120) { P += T - 120; T = 120; }
    var top, gapC, gapS, bottom;
    if (H >= 30) { top = 1; gapC = 1; gapS = 1; bottom = 1; } else if (H >= 20) { top = 0; gapC = 1; gapS = 0; bottom = 0; } else { top = 0; gapC = 0; gapS = 0; bottom = 0; }
    var compRows = o.composerHidden ? 0 : (H <= 12 ? 1 : 2);
    if (!compRows) gapC = 0;
    var statusRow = H - 1 - bottom, compTop = statusRow - gapS - compRows;
    return { W: W, H: H, T: T, P: P, focus: focus, top: top, bottom: bottom, gapC: gapC, gapS: gapS, statusRow: statusRow, compTop: compTop, compRows: compRows,
      tTop: top, tRows: compTop - gapC - top, paneX: 2 + T + 2, paneTop: top, paneRows: (compRows ? compTop + compRows : statusRow - gapS) - top, compact: H <= 12 };
  }

  // Compose a whole screen from a state spec (see lib/p1-screens.js for the fields).
  function compose(spec, W, H) {
    var grid = make(W, H);
    var focus = spec.focus != null ? spec.focus : H <= 12;
    var gm = geometry(W, H, { focus: focus, full: !!spec.full, paneWidth: spec.paneWidth, composerHidden: spec.full || (focus && spec.composerEmpty) });
    if (spec.status) blitRows(grid, 2, gm.statusRow, cells(spec.status(W - 4)));
    if (spec.full) {
      var fh = gm.statusRow - gm.gapS - gm.top;
      blitRows(grid, 2, gm.top, cells(spec.full(W - 4, fh)), fh);
      grid.geometry = gm; return grid;
    }
    var T = gm.T, area = gm.tRows, ctx = { compact: gm.compact, rows: area, H: H, W: W };
    var attach = spec.attach ? cells(spec.attach(T)) : [];
    var mark = spec.scrollMark ? cells(spec.scrollMark(T)) : [];
    var docked = spec.docked ? cells(spec.docked(T, ctx)) : [];
    var queued = spec.queued ? cells(spec.queued(T)) : [];
    var stack = mark.concat(docked, queued);
    var avail = area - attach.length - stack.length;
    var content = cells(spec.transcript ? spec.transcript(T, ctx) : []);
    var view;
    if (spec.scrollTop != null) view = content.slice(spec.scrollTop, spec.scrollTop + avail);
    else view = content.length > avail ? content.slice(content.length - avail) : content;
    blitRows(grid, 2, gm.tTop, attach);
    blitRows(grid, 2, gm.tTop + attach.length, view);
    blitRows(grid, 2, gm.tTop + area - stack.length, stack);
    if (spec.home) {
      var free = avail - view.length;
      if (free >= 14 && T >= 30) { var mono = cells(spec.home(T)); blitRows(grid, 2, gm.tTop + attach.length + view.length + Math.floor((free - mono.length) / 2), mono); }
    }
    if (gm.compRows && spec.composer) blitRows(grid, 2, gm.compTop, cells(spec.composer(T, { compact: gm.compRows === 1 })), gm.compRows);
    function paneInto(x, y, P, rowsN) {
      fill(grid, x, y, P, rowsN, "B");
      var inner = rowsN - 2;
      if (spec.pane) blitRows(grid, x, y + 1, cells(spec.pane(P, inner), "B"), inner);
      if (spec.paneStrip) blitRows(grid, x, y + rowsN - 1, cells(spec.paneStrip(P), "B"), 1);
      if (spec.peek) blitRows(grid, x, y + 1, cells(spec.peek(P), "B"), 2);
    }
    if (gm.P) paneInto(gm.paneX, gm.paneTop, gm.P, gm.paneRows);
    else if (spec.overlay && !focus) { var Po = Math.min(38, W - 4); paneInto(W - 2 - Po, gm.paneTop, Po, gm.paneRows); }
    grid.geometry = gm;
    return grid;
  }

  function toText(grid) { return grid.rows.map(function (r) { return r.map(function (c) { return c.ch; }).join(""); }); }
  function toRuns(grid) {
    return grid.rows.map(function (r) {
      var out = [], cur = null, n = 0;
      r.forEach(function (c) { var k = c.bg + c.fg; if (k === cur) n++; else { if (cur) out.push(cur + "\u00d7" + n); cur = k; n = 1; } });
      if (cur) out.push(cur + "\u00d7" + n);
      return out.join(" ");
    });
  }
  function pad2(n) { return (n < 10 ? "0" : "") + n; }
  function ruler(W) {
    var t = "", u = ""; for (var c = 0; c < W; c++) { t += c % 10 === 0 ? String(Math.floor(c / 10) % 10) : " "; u += String(c % 10); }
    return ["   " + t, "   " + u];
  }
  function toMarkdown(grid, title) {
    var text = toText(grid), runs = toRuns(grid);
    return (title ? title + "\n\n" : "") + "TEXT " + grid.W + "\u00d7" + grid.H + "\n```text\n" + ruler(grid.W).join("\n") + "\n" + text.map(function (l, i) { return pad2(i) + " " + l; }).join("\n") + "\n```\nRUNS\n```text\n" + runs.map(function (l, i) { return pad2(i) + " " + l; }).join("\n") + "\n```\n";
  }
  // A component-level mock: rows of one width on GROUND.
  function strip(rows, W) { var cs = cells(rows); var grid = make(W, cs.length); blitRows(grid, 0, 0, cs); return grid; }

  // 256-colour and NO_COLOR previews (handoff §2).
  var FG256 = { i: "#e4e4e4", d: "#9e9e9e", f: "#6c6c6c", a: "#d7af5f", x: "#d75f5f", o: "#87af5f", l: "#5fafaf", r: "#87afd7", s: "#d787d7", g: "#080808", u: "#262626", "+": "#d7ffd7", "-": "#ffd7d7", _: "#e4e4e4" };
  var BG256 = { G: "#080808", B: "#121212", P: "#1c1c1c", "+": "#005f00", "-": "#5f0000", A: "#d7af5f", N: "#e4e4e4" };
  function style(fg, bg, mode) {
    if (mode === "256") return { color: FG256[fg], background: BG256[bg] };
    if (mode === "NO_COLOR") {
      if (bg === "A" || bg === "N") return { color: "#0c0c0c", background: "#cfcfcf" };
      return { color: fg === "f" ? "#7a7a7a" : fg === "u" ? "transparent" : "#cfcfcf", background: "#0c0c0c" };
    }
    return { color: FG_CSS[fg], background: BG_CSS[bg] };
  }
  // GridView — renders a grid as terminal cells (the visual kit). `reduced` freezes ▪▪▪;
  // `mode` = "24-bit" | "256" | "NO_COLOR".
  function GridView(props) {
    var h = g.React.createElement, grid = props.grid, mode = props.mode || "24-bit";
    if (!grid) return null;
    return h("div", { style: { fontFamily: "var(--font-mono)", fontSize: props.fontSize || 12, lineHeight: 1.5, whiteSpace: "pre", background: mode === "NO_COLOR" ? "#0c0c0c" : "var(--h-ground)", width: grid.W + "ch", color: "var(--h-ink)", fontVariantLigatures: "none" } },
      grid.rows.map(function (r, y) {
        var spans = [], cur = null;
        r.forEach(function (c, x) {
          var key = c.bg + c.fg + (c.work != null ? "w" + c.work : "");
          if (cur && cur.key === key && c.work == null) cur.text += c.ch;
          else { cur = { key: key, text: c.ch, fg: c.fg, bg: c.bg, work: c.work }; spans.push(cur); }
        });
        return h("div", { key: y, style: { height: "1lh" } }, spans.map(function (s, i) {
          var st = style(s.fg, s.bg, mode);
          if (s.work != null && !props.reduced && mode !== "NO_COLOR") { st.animation = "h-work 1.1s infinite"; st.animationDelay = (s.work * 0.18) + "s"; }
          return h("span", { key: i, style: st }, s.text);
        }));
      }));
  }
  // GridCanvas — the same grid painted on one <canvas> (cheap: one node per screen). ▪▪▪ static.
  function paint(cv, grid, mode, fontSize) {
    var fs = fontSize || 12, lh = Math.round(fs * 1.5), dpr = (g.devicePixelRatio || 1);
    var ctx = cv.getContext("2d"), font = fs + "px 'JetBrains Mono', ui-monospace, monospace";
    ctx.font = font; var cw = ctx.measureText("M").width;
    cv.width = Math.ceil(grid.W * cw * dpr); cv.height = grid.H * lh * dpr;
    cv.style.width = (grid.W * cw) + "px"; cv.style.height = (grid.H * lh) + "px";
    ctx.scale(dpr, dpr); ctx.font = font; ctx.textBaseline = "middle";
    var css = g.getComputedStyle ? g.getComputedStyle(g.document.documentElement) : null, memo = {};
    function resolve(v) { if (memo[v] != null) return memo[v]; var m = /var\((--[^)]+)\)/.exec(v || ""); return (memo[v] = m && css ? css.getPropertyValue(m[1]).trim() || "#e8e8e8" : v); }
    var last = null;
    grid.rows.forEach(function (r, y) {
      r.forEach(function (c, x) {
        var k = c.fg + c.bg, st = last && last.k === k ? last : (last = { k: k, s: style(c.fg, c.bg, mode) });
        ctx.fillStyle = resolve(st.s.background); ctx.fillRect(x * cw, y * lh, cw + 0.5, lh);
        if (c.ch !== " ") { ctx.fillStyle = resolve(st.s.color); ctx.fillText(c.ch, x * cw, y * lh + lh / 2); }
      });
    });
  }
  function GridCanvas(props) {
    var h = g.React.createElement, ref = g.React.useRef(null);
    g.React.useEffect(function () {
      var cv = ref.current; if (!cv || !props.grid) return;
      paint(cv, props.grid, props.mode, props.fontSize);
      if (g.document && g.document.fonts) g.document.fonts.ready.then(function () { if (ref.current) paint(ref.current, props.grid, props.mode, props.fontSize); });
    }, [props.grid, props.mode]);
    return h("canvas", { ref: ref, style: { display: "block" } });
  }
  g.P1Cells = { band: band, cells: cells, make: make, blit: blit, blitRows: blitRows, fill: fill, geometry: geometry, compose: compose, toText: toText, toRuns: toRuns, toMarkdown: toMarkdown, strip: strip, GridView: GridView, GridCanvas: GridCanvas };
})();
