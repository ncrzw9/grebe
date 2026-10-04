// The results panel's page. Runs inside the webview; talks to the extension
// only through postMessage.
//
// Each result is a canvas grid: one <canvas> pinned over a native scrolling
// area, repainted per frame with only the rows *and columns* in view -- no
// per-cell DOM at all, so 100,000 rows or 200 columns cost the same to
// scroll. Data arrives column-major as display text (null = NULL).
//
//   click / drag            select cells; click a row number: whole rows
//   arrows, Home/End, PgUp/PgDn, Cmd/Ctrl+Home/End     move
//   Shift + any of those    extend the selection
//   Cmd/Ctrl+A, Cmd/Ctrl+C  select all, copy as TSV (header row included
//                           when whole columns are selected; NULL copies
//                           as an empty cell, as spreadsheets expect)
//   click / drag headers    select whole columns (Shift+click extends)
//   Ctrl+Space, Shift+Space select the current column(s) / row(s)
//   Cmd/Ctrl+arrows         jump to the edge (with Shift: extend to it)
//   the ↕ at a header's right edge   sort asc / desc / off (NULLs last)
//   drag a header edge      resize; double-click it to fit the content

(function () {
  "use strict";

  const vscode = acquireVsCodeApi();
  const ROW = 20;
  const HEAD = 22;
  const PAD = 6;
  const SORTW = 16; // the sort toggle at the right of each header

  const $ = (tag, cls, text) => {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text !== undefined) e.textContent = text;
    return e;
  };

  // --- theme --------------------------------------------------------------
  let theme = null;
  function readTheme() {
    const s = getComputedStyle(document.body);
    const v = (name, fallback) => s.getPropertyValue(name).trim() || fallback;
    const size = parseFloat(v("--vscode-editor-font-size", "12")) || 12;
    const family = v("--vscode-editor-font-family", "monospace");
    theme = {
      font: `${size}px ${family}`,
      bold: `600 ${size}px ${family}`,
      small: `${Math.max(size - 2, 9)}px ${family}`,
      italic: `italic ${size}px ${family}`,
      fg: v("--vscode-editor-foreground", v("--vscode-foreground", "#ccc")),
      muted: v("--vscode-descriptionForeground", "#888"),
      bg: v("--vscode-editor-background", "#1e1e1e"),
      head: v("--vscode-editorWidget-background", "#252526"),
      line: v("--vscode-editorWidget-border", v("--vscode-panel-border", "#3c3c3c")),
      sel: v("--vscode-editor-selectionBackground", "#264f78"),
      focus: v("--vscode-focusBorder", "#007fd4"),
    };
    for (const g of grids) g.repaint(true);
  }
  const grids = new Set();
  new MutationObserver(() => readTheme()).observe(document.body, { attributes: true, attributeFilter: ["class"] });

  // --- messages -----------------------------------------------------------
  window.addEventListener("message", ({ data }) => {
    const runs = document.getElementById("runs");
    if (!theme) readTheme();
    if (data.type === "begin") {
      for (const g of grids) g.dispose();
      grids.clear();
      runs.replaceChildren();
      document.getElementById("title").textContent = data.header.title;
      document.getElementById("summary").textContent = data.header.detail;
    } else if (data.type === "end") {
      document.getElementById("summary").textContent = data.summary;
    } else if (data.type === "result") {
      runs.append(section(data.entry));
    } else if (data.type === "exported") {
      const note = document.querySelector(`[data-export-note="${data.id}"]`);
      if (note) note.textContent = data.message;
    }
  });

  function section(entry) {
    const sec = $("section");
    const stmt = $("div", "stmt");
    // A statement from a .sql file links back to it; an inspection of a
    // data file has no line to go to.
    if (typeof entry.line === "number") {
      const link = $("a", "", "L" + (entry.line + 1));
      link.title = "Go to the statement";
      link.addEventListener("click", () => vscode.postMessage({ type: "reveal", uri: entry.uri, line: entry.line }));
      stmt.append(link, document.createTextNode(" "));
    }
    stmt.append(document.createTextNode(entry.preview));
    sec.append(stmt);

    const r = entry.result;
    const ms = r.ms === undefined ? "" : fmtMs(r.ms);
    if (r.kind === "error") {
      sec.append($("div", "error", r.type + " Error: " + r.message));
      // Where in the statement DuckDB stopped: the line, and a caret.
      if (r.context) {
        const n = String(r.context.line + 1);
        sec.append($("pre", "context", `${n} | ${r.context.text}\n${" ".repeat(n.length)} | ${" ".repeat(r.context.column)}^`));
      }
    } else if (r.kind === "text") {
      sec.append($("pre", "text", r.text));
      sec.append($("div", "note", ms));
    } else if (r.kind === "ok") {
      sec.append($("div", "note", "OK · " + ms));
    } else {
      const shown = r.data.length ? r.data[0].length : 0;
      const bar = $("div", "bar");
      const count =
        (r.more
          ? `first ${shown.toLocaleString()}`
          : shown < r.total
            ? `${shown.toLocaleString()} of ${r.total.toLocaleString()}`
            : r.total.toLocaleString()) +
        (r.total === 1 && !r.more ? " row" : " rows") +
        ` × ${r.columns.length} · ${ms}`;
      bar.append($("span", "note", count));
      if (entry.exportable) {
        const exp = $("span", "export");
        exp.append($("span", "note", "export"));
        for (const [fmt, label] of [["csv", "csv"], ["tsv", "tsv"], ["parquet", "parquet"], ["json", "json"]]) {
          const b = $("button", "", label);
          b.title =
            "Save the full result with DuckDB's COPY: every row, exact types. " +
            "The query runs again, so now() or random() may differ from what is shown.";
          b.addEventListener("click", () => vscode.postMessage({ type: "export", id: entry.id, format: fmt }));
          exp.append(b);
        }
        const note = $("span", "note");
        note.dataset.exportNote = String(entry.id);
        exp.append(note);
        bar.append(exp);
      }
      sec.append(bar);
      if (r.columns.length) sec.append(grid(r));
    }
    return sec;
  }

  const fmtMs = (ms) => (ms < 10 ? ms.toFixed(1) : Math.round(ms).toLocaleString()) + " ms";

  // --- the grid -----------------------------------------------------------
  function grid(r) {
    const cols = r.columns;
    const types = r.types || [];
    const data = r.data; // data[c][row] : string | null
    const num = r.num || [];
    const n = data.length ? data[0].length : 0;

    const root = $("div", "grid");
    root.tabIndex = 0;
    root.setAttribute("role", "grid");
    root.setAttribute("aria-label", `${n} rows, ${cols.length} columns`);
    const viewport = $("div", "viewport");
    const canvas = $("canvas");
    const sizer = $("div", "sizer");
    viewport.append(canvas, sizer);
    root.append(viewport);
    const ctx = canvas.getContext("2d", { alpha: false });

    // Row-number column wide enough for the last row number.
    ctx.font = theme.small;
    const NUMW = Math.ceil(ctx.measureText(String(n)).width) + PAD * 2 + 4;

    // Widths: header and the first 1,000 values, clamped.
    const widths = cols.map((name, c) => fit(c, 1000));
    function fit(c, sample) {
      ctx.font = theme.bold;
      // Room for the sort arrow too, so sorting never pushes the type out.
      let w = ctx.measureText(cols[c] + " ▾").width;
      ctx.font = theme.small;
      if (types[c]) w += ctx.measureText(" " + types[c]).width + 6;
      ctx.font = theme.font;
      const col = data[c];
      let widest = 0;
      for (let k = 0, m = Math.min(n, sample); k < m; k++) {
        const t = col[k];
        if (t !== null && t.length > widest) {
          // Measure only strings longer than any seen so far: the editor
          // font is monospace, so a longer string is never narrower.
          widest = t.length;
          w = Math.max(w, ctx.measureText(t).width);
        }
      }
      return Math.min(Math.max(Math.ceil(w) + PAD * 2 + 2, 44), 380);
    }
    let x = [];
    const layout = () => {
      x = [NUMW];
      for (const w of widths) x.push(x[x.length - 1] + w);
      sizer.style.width = x[x.length - 1] + "px";
      sizer.style.height = HEAD + n * ROW + "px";
    };
    layout();
    viewport.style.height = Math.min(HEAD + n * ROW + 16, Math.max(180, Math.round(window.innerHeight * 0.6))) + "px";

    // View order (sorting) and selection, both in view coordinates.
    let order = null; // null = natural order
    let sort = { col: -1, dir: 0 };
    const at = (v) => (order ? order[v] : v);
    let sel = null; // { ar, ac, r, c } anchor + active, inclusive
    let hoverCol = -1; // header under the mouse, for the sort toggle
    const range = () =>
      sel && [Math.min(sel.ar, sel.r), Math.max(sel.ar, sel.r), Math.min(sel.ac, sel.c), Math.max(sel.ac, sel.c)];

    // --- painting ---------------------------------------------------------
    let dirty = false;
    let W = 0;
    let H = 0;
    let dpr = 1;
    function size() {
      W = viewport.clientWidth;
      H = viewport.clientHeight;
      dpr = window.devicePixelRatio || 1;
      canvas.width = Math.max(1, Math.round(W * dpr));
      canvas.height = Math.max(1, Math.round(H * dpr));
      canvas.style.width = W + "px";
      canvas.style.height = H + "px";
      sizer.style.marginTop = -H + "px";
    }
    function repaint(resize) {
      if (resize) size();
      if (dirty) return;
      dirty = true;
      requestAnimationFrame(paint);
    }

    function paint() {
      dirty = false;
      const t = theme;
      const sx = viewport.scrollLeft;
      const sy = viewport.scrollTop;
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      ctx.fillStyle = t.bg;
      ctx.fillRect(0, 0, W, H);
      ctx.textBaseline = "middle";

      const first = Math.floor(sy / ROW);
      const last = Math.min(n, Math.ceil((sy + H - HEAD) / ROW) + 1);
      let c0 = 0;
      while (c0 < cols.length && x[c0 + 1] - sx <= NUMW) c0++;
      let c1 = c0;
      while (c1 < cols.length && x[c1] - sx < W) c1++;
      const sr = range();

      // Selection background.
      if (sr) {
        ctx.fillStyle = t.sel;
        const ra = Math.max(sr[0], first);
        const rb = Math.min(sr[1], last - 1);
        const left = Math.max(x[sr[2]] - sx, NUMW);
        const right = Math.min(x[sr[3] + 1] - sx, W);
        if (rb >= ra && right > left) ctx.fillRect(left, HEAD + ra * ROW - sy, right - left, (rb - ra + 1) * ROW);
      }

      // Cells, one clip per column.
      for (let c = c0; c < c1; c++) {
        const left = x[c] - sx;
        const w = widths[c];
        ctx.save();
        ctx.beginPath();
        ctx.rect(Math.max(left, NUMW), HEAD, Math.min(w, left + w - NUMW), H - HEAD);
        ctx.clip();
        const col = data[c];
        const right = num[c];
        ctx.textAlign = right ? "right" : "left";
        const tx = right ? left + w - PAD : left + PAD;
        for (let v = first; v < last; v++) {
          const y = HEAD + v * ROW - sy + ROW / 2;
          const val = col[at(v)];
          if (val === null) {
            ctx.font = t.italic;
            ctx.fillStyle = t.muted;
            ctx.fillText("NULL", tx, y);
          } else {
            ctx.font = t.font;
            ctx.fillStyle = t.fg;
            ctx.fillText(val, tx, y);
          }
        }
        ctx.restore();
      }

      // Grid lines: hairlines between rows and columns.
      ctx.strokeStyle = t.line;
      ctx.lineWidth = 1 / dpr;
      ctx.beginPath();
      for (let v = first; v <= last; v++) {
        const y = Math.round((HEAD + v * ROW - sy) * dpr) / dpr + 0.5 / dpr;
        ctx.moveTo(NUMW, y);
        ctx.lineTo(Math.min(x[cols.length] - sx, W), y);
      }
      for (let c = c0; c <= c1 && c <= cols.length; c++) {
        const xx = Math.round((x[c] - sx) * dpr) / dpr + 0.5 / dpr;
        if (xx < NUMW) continue;
        ctx.moveTo(xx, 0);
        ctx.lineTo(xx, Math.min(HEAD + n * ROW - sy, H));
      }
      ctx.stroke();

      // Active cell.
      if (sel && sel.r >= first && sel.r < last && sel.c >= c0 && sel.c < c1 && document.activeElement === root) {
        ctx.strokeStyle = t.focus;
        ctx.lineWidth = 2;
        ctx.strokeRect(x[sel.c] - sx + 1, HEAD + sel.r * ROW - sy + 1, widths[sel.c] - 2, ROW - 2);
      }

      // Row numbers.
      ctx.fillStyle = t.head;
      ctx.fillRect(0, HEAD, NUMW, H - HEAD);
      ctx.font = t.small;
      ctx.textAlign = "right";
      for (let v = first; v < last; v++) {
        const inSel = sr && v >= sr[0] && v <= sr[1];
        ctx.fillStyle = inSel ? t.fg : t.muted;
        ctx.fillText(String(v + 1), NUMW - PAD, HEAD + v * ROW - sy + ROW / 2);
      }

      // Header.
      ctx.fillStyle = t.head;
      ctx.fillRect(0, 0, W, HEAD);
      for (let c = c0; c < c1; c++) {
        const left = x[c] - sx;
        const w = widths[c];
        ctx.save();
        ctx.beginPath();
        ctx.rect(Math.max(left, NUMW), 0, Math.min(w, left + w - NUMW), HEAD);
        ctx.clip();
        if (sr && sr[0] === 0 && sr[1] === n - 1 && c >= sr[2] && c <= sr[3]) {
          ctx.fillStyle = t.sel;
          ctx.fillRect(left, 0, w, HEAD);
        }
        ctx.textAlign = "left";
        ctx.font = t.bold;
        ctx.fillStyle = t.fg;
        ctx.fillText(cols[c], left + PAD, HEAD / 2);
        if (types[c]) {
          const lw = ctx.measureText(cols[c]).width;
          ctx.font = t.small;
          ctx.fillStyle = t.muted;
          ctx.fillText(types[c], left + PAD + lw + 6, HEAD / 2 + 1);
        }
        // Sort toggle: always shown on the sorted column, on hover otherwise.
        const glyph = sort.col === c ? (sort.dir > 0 ? "▴" : "▾") : hoverCol === c ? "↕" : "";
        if (glyph) {
          ctx.fillStyle = t.head;
          ctx.fillRect(left + w - SORTW - 4, 1, SORTW, HEAD - 2);
          ctx.font = t.font;
          ctx.fillStyle = sort.col === c ? t.fg : t.muted;
          ctx.textAlign = "center";
          ctx.fillText(glyph, left + w - SORTW / 2 - 4, HEAD / 2);
        }
        ctx.restore();
      }
      ctx.strokeStyle = t.line;
      ctx.beginPath();
      ctx.moveTo(0, HEAD - 0.5);
      ctx.lineTo(W, HEAD - 0.5);
      ctx.moveTo(NUMW - 0.5, 0);
      ctx.lineTo(NUMW - 0.5, H);
      ctx.stroke();
    }

    // --- hit testing ------------------------------------------------------
    function colAt(px) {
      const cx = px + viewport.scrollLeft;
      for (let c = 0; c < cols.length; c++) if (cx < x[c + 1]) return c;
      return cols.length - 1;
    }
    const rowAt = (py) => Math.min(n - 1, Math.max(0, Math.floor((py - HEAD + viewport.scrollTop) / ROW)));
    function edgeAt(px) {
      const cx = px + viewport.scrollLeft;
      for (let c = 0; c < cols.length; c++) if (Math.abs(cx - x[c + 1]) <= 4) return c;
      return -1;
    }

    const inSortZone = (px, c) => {
      const cx = px + viewport.scrollLeft;
      return cx >= x[c + 1] - 4 - SORTW && cx < x[c + 1] - 4;
    };
    // Whole columns ac..c; the active cell sits at the top.
    function selectColumns(ac, c) {
      if (!n) return;
      sel = { ar: n - 1, ac, r: 0, c };
      repaint();
    }

    // --- selection & movement --------------------------------------------
    function reveal() {
      const y = sel.r * ROW;
      if (y < viewport.scrollTop) viewport.scrollTop = y;
      else if (y + ROW > viewport.scrollTop + H - HEAD) viewport.scrollTop = y + ROW - (H - HEAD);
      const l = x[sel.c] - NUMW;
      const rr = x[sel.c + 1];
      if (l < viewport.scrollLeft) viewport.scrollLeft = l;
      else if (rr > viewport.scrollLeft + W) viewport.scrollLeft = rr - W;
    }
    function moveTo(v, c, extend) {
      v = Math.min(Math.max(v, 0), n - 1);
      c = Math.min(Math.max(c, 0), cols.length - 1);
      sel = extend && sel ? { ar: sel.ar, ac: sel.ac, r: v, c } : { ar: v, ac: c, r: v, c };
      reveal();
      repaint();
    }

    function copy() {
      if (!sel) return;
      const [r0, r1, c0, c1] = range();
      const clean = (s) => (s === null ? "" : s.replace(/[\t\r\n]+/g, " "));
      const lines = [];
      if (r0 === 0 && r1 === n - 1) lines.push(cols.slice(c0, c1 + 1).map(clean).join("\t"));
      for (let v = r0; v <= r1; v++) {
        const row = at(v);
        const cells = [];
        for (let c = c0; c <= c1; c++) cells.push(clean(data[c][row]));
        lines.push(cells.join("\t"));
      }
      vscode.postMessage({ type: "copy", text: lines.join("\n") });
    }

    function sortBy(c) {
      const dir = sort.col === c ? (sort.dir === 1 ? -1 : sort.dir === -1 ? 0 : 1) : 1;
      sort = { col: dir ? c : -1, dir };
      if (!dir) {
        order = null;
      } else {
        const col = data[c];
        const keys = num[c] ? Float64Array.from(col, (s) => (s === null ? NaN : Number(s))) : col;
        const idx = new Int32Array(n);
        for (let k = 0; k < n; k++) idx[k] = k;
        const numeric = num[c];
        const cmp = (a, b) => {
          const ka = keys[a];
          const kb = keys[b];
          const na = numeric ? Number.isNaN(ka) : ka === null;
          const nb = numeric ? Number.isNaN(kb) : kb === null;
          if (na || nb) return na === nb ? a - b : na ? 1 : -1; // NULLs last
          const d = numeric ? ka - kb : ka < kb ? -1 : ka > kb ? 1 : 0;
          return d * dir || a - b;
        };
        order = Array.from(idx).sort(cmp);
      }
      repaint();
    }

    // --- events -----------------------------------------------------------
    let dragging = null;
    canvas.addEventListener("mousedown", (e) => {
      e.preventDefault();
      root.focus();
      const px = e.offsetX;
      const py = e.offsetY;
      if (py < HEAD) {
        const edge = edgeAt(px);
        if (edge >= 0) {
          if (e.detail === 2) {
            widths[edge] = fit(edge, n);
            layout();
            repaint();
            return;
          }
          const start = e.clientX;
          const w0 = widths[edge];
          dragging = { move: (ev) => { widths[edge] = Math.max(32, w0 + ev.clientX - start); layout(); repaint(); } };
        } else if (px >= NUMW) {
          const c = colAt(px);
          if (inSortZone(px, c)) return sortBy(c);
          const from = e.shiftKey && sel ? sel.ac : c;
          selectColumns(from, c);
          dragging = {
            move: (ev) => {
              const b = canvas.getBoundingClientRect();
              selectColumns(from, colAt(Math.max(NUMW, ev.clientX - b.left)));
            },
          };
        }
        return;
      }
      if (n === 0) return;
      const v = rowAt(py);
      if (px < NUMW) {
        // Row numbers select whole rows.
        sel = e.shiftKey && sel ? { ar: sel.ar, ac: 0, r: v, c: cols.length - 1 } : { ar: v, ac: 0, r: v, c: cols.length - 1 };
        repaint();
        dragging = { move: (ev) => { const b = canvas.getBoundingClientRect(); sel.r = rowAt(ev.clientY - b.top); repaint(); } };
        return;
      }
      moveTo(v, colAt(px), e.shiftKey);
      dragging = {
        move: (ev) => {
          const b = canvas.getBoundingClientRect();
          sel.r = rowAt(ev.clientY - b.top);
          sel.c = colAt(Math.max(NUMW, ev.clientX - b.left));
          repaint();
        },
      };
    });
    window.addEventListener("mousemove", (e) => {
      if (dragging) return dragging.move(e);
      if (e.target !== canvas) return;
      const px = e.offsetX;
      const py = e.offsetY;
      canvas.style.cursor = py < HEAD && edgeAt(px) >= 0 ? "col-resize" : py < HEAD && px >= NUMW ? "pointer" : "default";
      const hc = py < HEAD && px >= NUMW ? colAt(px) : -1;
      if (hc !== hoverCol) {
        hoverCol = hc;
        repaint();
      }
      // Full text of a clipped cell on hover.
      if (py >= HEAD && px >= NUMW && n) {
        const c = colAt(px);
        const val = data[c][at(rowAt(py))];
        ctx.font = theme.font;
        canvas.title = val !== null && ctx.measureText(val).width > widths[c] - PAD * 2 ? val : "";
      } else if (py < HEAD && px >= NUMW) {
        const c = colAt(px);
        canvas.title = inSortZone(px, c)
          ? "Sort ascending / descending / off"
          : cols[c] + (types[c] ? " · " + types[c] : "") + " — click to select the column, Shift+click to extend";
      } else {
        canvas.title = "";
      }
    });
    window.addEventListener("mouseup", () => (dragging = null));
    canvas.addEventListener("mouseleave", () => {
      if (hoverCol !== -1) {
        hoverCol = -1;
        repaint();
      }
    });

    root.addEventListener("keydown", (e) => {
      const mod = e.metaKey || e.ctrlKey;
      if (mod && e.key.toLowerCase() === "a") {
        e.preventDefault();
        if (n) sel = { ar: 0, ac: 0, r: n - 1, c: cols.length - 1 };
        return repaint();
      }
      if (mod && e.key.toLowerCase() === "c") {
        e.preventDefault();
        return copy();
      }
      const cur = sel || { ar: 0, ac: 0, r: 0, c: 0 };
      if (e.key === " " && n && (e.ctrlKey || e.shiftKey)) {
        e.preventDefault();
        // Ctrl+Space: the current column(s); Shift+Space: the current row(s).
        if (e.ctrlKey) selectColumns(cur.ac, cur.c);
        else {
          sel = { ar: cur.ar, ac: 0, r: cur.r, c: cols.length - 1 };
          repaint();
        }
        return;
      }
      const page = Math.max(1, Math.floor((H - HEAD) / ROW) - 1);
      const go = {
        ArrowUp: mod ? [0, cur.c] : [cur.r - 1, cur.c],
        ArrowDown: mod ? [n - 1, cur.c] : [cur.r + 1, cur.c],
        ArrowLeft: mod ? [cur.r, 0] : [cur.r, cur.c - 1],
        ArrowRight: mod ? [cur.r, cols.length - 1] : [cur.r, cur.c + 1],
        PageUp: [cur.r - page, cur.c],
        PageDown: [cur.r + page, cur.c],
        Home: mod ? [0, 0] : [cur.r, 0],
        End: mod ? [n - 1, cols.length - 1] : [cur.r, cols.length - 1],
      }[e.key];
      if (go && n) {
        e.preventDefault();
        moveTo(go[0], go[1], e.shiftKey);
      }
    });
    root.addEventListener("focus", () => repaint());
    root.addEventListener("blur", () => repaint());
    viewport.addEventListener("scroll", () => repaint(), { passive: true });
    const ro = new ResizeObserver(() => repaint(true));
    ro.observe(viewport);

    const g = {
      repaint: (resize) => repaint(resize),
      dispose: () => ro.disconnect(),
    };
    grids.add(g);
    requestAnimationFrame(() => repaint(true));
    return root;
  }

  // Loaded (first time, or again after the view was moved): the host
  // replays whatever is on screen now.
  vscode.postMessage({ type: "ready" });
})();
