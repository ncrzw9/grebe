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

  // --- layout -------------------------------------------------------------
  //
  // One run on one page: a header line; a tab strip with a tab for each
  // result set and one for Messages (every statement echoed, with its
  // outcome, and the error if any); the active tab filling the page; and a
  // status line for a result. A filter at the right of the tab strip
  // narrows the grid shown.
  const el = (id) => document.getElementById(id);
  let tabs = []; // { kind: "result"|"messages", entry, button, view, dispose, note }
  let active = -1;
  let messages = null; // the Messages tab
  let errors = 0;

  const fmtMs = (ms) => {
    if (ms === undefined || ms === null) return "";
    if (ms < 10) return ms.toFixed(1) + " ms";
    if (ms < 10000) return Math.round(ms).toLocaleString() + " ms";
    if (ms < 60000) return (ms / 1000).toFixed(1) + " s";
    const s = Math.round(ms / 1000);
    return `${Math.floor(s / 60)} min ${s % 60} s`;
  };

  function reset(header) {
    stopRunning();
    for (const t of tabs) if (t.dispose) t.dispose();
    tabs = [];
    active = -1;
    errors = 0;
    el("title").textContent = header.title;
    el("summary").textContent = header.detail || "";
    for (const id of ["tablist", "main", "status"]) el(id).replaceChildren();
    el("tabs").hidden = true;
    el("filter").hidden = true;
    el("filter-text").value = "";
    messages = { kind: "messages", button: $("button", "tab", "Messages"), view: $("div", "messages"), entry: null };
    messages.button.addEventListener("click", () => show(tabs.indexOf(messages)));
  }

  /** A statement's line link, back to where it is in the file. */
  function lineLink(entry) {
    if (typeof entry.line !== "number") return null;
    const a = $("a", "ln", "L" + (entry.line + 1));
    a.title = "Go to the statement";
    a.addEventListener("click", () => vscode.postMessage({ type: "reveal", uri: entry.uri, line: entry.line }));
    return a;
  }

  /** One line in Messages: a mark, the line link, the statement, the outcome. */
  function message(entry, cls, mark, outcome) {
    const line = $("div", "line " + cls);
    line.append($("span", "mark", mark));
    const link = lineLink(entry);
    if (link) line.append(link);
    const sql = $("span", "sql", entry.preview);
    sql.title = entry.preview;
    line.append(sql, $("span", "outcome", outcome));
    messages.view.append(line);
    return line;
  }

  function rowsLabel(r) {
    const shown = r.data.length ? r.data[0].length : 0;
    if (r.more) return `${shown.toLocaleString()}+ rows`;
    if (shown < r.total) return `${shown.toLocaleString()} of ${r.total.toLocaleString()} rows`;
    return `${r.total.toLocaleString()} row${r.total === 1 ? "" : "s"}`;
  }

  function addResult(entry) {
    const r = entry.result;
    if (!tabs.includes(messages)) {
      tabs.push(messages);
      el("tablist").append(messages.button);
    }
    if (r.kind === "ok") {
      message(entry, "ok", "✓", fmtMs(r.ms));
    } else if (r.kind === "error") {
      errors++;
      message(entry, "error", "✗", "");
      messages.view.append($("div", "detail error", r.type + " Error: " + r.message));
      // Where in the statement DuckDB stopped: the line, and a caret.
      if (r.context) {
        const n = String(r.context.line + 1);
        messages.view.append($("pre", "context", `${n} | ${r.context.text}\n${" ".repeat(n.length)} | ${" ".repeat(r.context.column)}^`));
      }
    } else if (r.kind === "cancelled") {
      const why = r.reason === "timeout" ? "Timed out (grebe.duckdb.queryTimeout)" : "Cancelled";
      message(entry, "cancelled", "■", fmtMs(r.ms));
      messages.view.append($("div", "detail cancelled", `${why}${r.ms ? " after " + fmtMs(r.ms) : ""}.` + (r.ended ? " " + r.message : "")));
    } else {
      message(entry, "ok", "✓", `${r.kind === "rows" ? rowsLabel(r) : "text"} · ${fmtMs(r.ms)}`);
      addTab(entry);
      return;
    }
    messages.button.textContent = errors ? `Messages (${errors} error${errors === 1 ? "" : "s"})` : "Messages";
    // An error or a stop: Messages is where the answer is.
    if (r.kind !== "ok" || !tabs.some((t) => t.kind === "result")) show(tabs.indexOf(messages));
    else refreshTabs();
  }

  // --- tabs -----------------------------------------------------------------
  function addTab(entry) {
    const r = entry.result;
    const button = $("button", "tab");
    button.textContent = (typeof entry.line === "number" ? `L${entry.line + 1} · ` : "") + (r.kind === "rows" ? rowsLabel(r) : "text");
    button.title = entry.preview;
    const t = { kind: "result", entry, button, view: null, dispose: null, note: "" };
    button.addEventListener("click", () => show(tabs.indexOf(t)));
    // Result tabs before Messages, in the order they ran.
    const at = tabs.indexOf(messages);
    tabs.splice(at, 0, t);
    el("tablist").insertBefore(button, messages.button);
    show(tabs.indexOf(t));
  }

  /** The strip holds the tabs and the filter. A data file's page (one
   *  result, nothing to report) needs no tabs, only the filter. */
  function refreshTabs() {
    el("tabs").hidden = tabs.length === 0;
    const results = tabs.filter((t) => t.kind === "result");
    const quiet = errors === 0 && !messages.view.querySelector(".cancelled");
    el("tablist").hidden = quiet && results.length === 1 && typeof results[0].entry.line !== "number";
  }

  function show(i) {
    if (i < 0 || !tabs[i]) return;
    if (active >= 0 && tabs[active]) tabs[active].button.classList.remove("active");
    active = i;
    const t = tabs[i];
    t.button.classList.add("active");
    if (t.kind === "result" && !t.view) {
      const r = t.entry.result;
      if (r.kind === "rows" && r.columns.length) {
        t.view = grid(r);
        t.dispose = t.view.dispose;
      } else if (r.kind === "rows") {
        t.view = $("div", "empty", "No columns.");
      } else {
        t.view = $("pre", "text", r.text);
      }
    }
    el("main").replaceChildren(t.view);
    if (t.view.repaint) requestAnimationFrame(() => t.view.repaint(true));
    // The filter belongs to a grid; each tab keeps its own.
    const isGrid = Boolean(t.view && t.view.filter);
    el("filter").hidden = !isGrid;
    if (isGrid) {
      t.filterState = t.filterState || { text: "", mode: "contains", col: -1 };
      fillFilter(t);
    }
    refreshTabs();
    status(t);
  }

  /** The status line: a result's size and time, and what can be done with it. */
  function status(t) {
    const bar = el("status");
    bar.replaceChildren();
    if (t.kind !== "result") {
      bar.hidden = true;
      return;
    }
    bar.hidden = false;
    const r = t.entry.result;
    let label = r.kind === "rows" ? `${rowsLabel(r)} × ${r.columns.length}` : "text";
    if (t.matched !== undefined && t.filterState && t.filterState.text) {
      label = `${t.matched.toLocaleString()} of ${rowsLabel(r)} match × ${r.columns.length}`;
    }
    const count = $("span", "count", label);
    bar.append(count, $("span", "ms", fmtMs(r.ms)), $("span", "spacer"));
    const actions = $("span", "actions");
    if (t.entry.exportable && r.more) {
      const b = $("button", "", "Count");
      b.title = "Count every row of the result (runs the query again)";
      b.addEventListener("click", () => {
        b.disabled = true;
        vscode.postMessage({ type: "count", id: t.entry.id });
      });
      actions.append(b);
    }
    if (t.entry.exportable) {
      for (const [fmt, name] of [["csv", "CSV"], ["tsv", "TSV"], ["parquet", "Parquet"], ["json", "JSON"]]) {
        const b = $("button", "", name);
        b.title = `Save every row as ${name} with DuckDB's COPY (runs the query again; the filter does not apply)`;
        b.addEventListener("click", () => vscode.postMessage({ type: "export", id: t.entry.id, format: fmt }));
        actions.append(b);
      }
    }
    const note = $("span", "note");
    note.dataset.exportNote = String(t.entry.id);
    note.textContent = t.note || "";
    actions.append(note);
    bar.append(actions);
  }

  // --- the filter -------------------------------------------------------------
  // Matches as you type, not case-sensitive; "contains" unless changed.
  const MATCHERS = {
    contains: (q) => (v) => v.toLowerCase().includes(q),
    equals: (q) => (v) => v.toLowerCase() === q,
    starts: (q) => (v) => v.toLowerCase().startsWith(q),
    regex: (q, raw) => {
      const re = new RegExp(raw, "i");
      return (v) => re.test(v);
    },
  };

  function fillFilter(t) {
    const st = t.filterState;
    el("filter-text").value = st.text;
    el("filter-mode").value = st.mode;
    const sel = el("filter-col");
    sel.replaceChildren($("option", "", "all columns"));
    sel.firstChild.value = "-1";
    t.entry.result.columns.forEach((c, i) => {
      const o = $("option", "", c);
      o.value = String(i);
      sel.append(o);
    });
    sel.value = String(st.col);
  }

  let filterTimer = null;
  function applyFilter() {
    const t = tabs[active];
    if (!t || !t.view || !t.view.filter) return;
    const st = t.filterState;
    st.text = el("filter-text").value;
    st.mode = el("filter-mode").value;
    st.col = Number(el("filter-col").value);
    el("filter-text").classList.remove("invalid");
    let match = null;
    if (st.text) {
      try {
        match = MATCHERS[st.mode](st.text.toLowerCase(), st.text);
      } catch {
        el("filter-text").classList.add("invalid"); // a regex that does not compile
        return;
      }
    }
    t.matched = t.view.filter(match, st.col);
    status(t);
  }
  const later = () => {
    clearTimeout(filterTimer);
    filterTimer = setTimeout(applyFilter, 60);
  };
  el("filter-text").addEventListener("input", later);
  el("filter-mode").addEventListener("change", applyFilter);
  el("filter-col").addEventListener("change", applyFilter);
  el("filter-text").addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      el("filter-text").value = "";
      applyFilter();
    } else if (e.key === "Enter" || e.key === "ArrowDown") {
      const t = tabs[active];
      if (t && t.view && t.view.focusGrid) t.view.focusGrid();
    }
  });
  // Cmd/Ctrl+F anywhere on the page: to the filter.
  window.addEventListener("keydown", (e) => {
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "f" && !el("filter").hidden) {
      e.preventDefault();
      el("filter-text").focus();
      el("filter-text").select();
    }
  });

  // --- what is running now, with a way to stop it --------------------------
  let ticking = null;
  function running(label, since) {
    stopRunning();
    const box = $("span", "running");
    const text = $("span", "note");
    const stop = $("button", "cancel", "Cancel");
    stop.title = "Stop this statement. The session and its TEMP tables stay.";
    stop.addEventListener("click", () => {
      stop.disabled = true;
      stop.textContent = "Cancelling…";
      vscode.postMessage({ type: "cancel" });
    });
    box.append(text, stop);
    el("bar").append(box);
    const tick = () => {
      const s = Math.max(0, Math.floor((Date.now() - since) / 1000));
      text.textContent = `${label} · ${s < 60 ? s + " s" : Math.floor(s / 60) + " min " + (s % 60) + " s"}`;
    };
    tick();
    ticking = { box, timer: setInterval(tick, 1000) };
  }
  function stopRunning() {
    if (!ticking) return;
    clearInterval(ticking.timer);
    ticking.box.remove();
    ticking = null;
  }

  // --- messages -----------------------------------------------------------
  window.addEventListener("message", ({ data }) => {
    if (!theme) readTheme();
    if (data.type === "begin") {
      reset(data.header);
    } else if (data.type === "running") {
      running(data.label, data.since);
    } else if (data.type === "result") {
      stopRunning();
      addResult(data.entry);
    } else if (data.type === "end") {
      stopRunning();
      el("summary").textContent = data.summary;
      if (tabs.length === 0) {
        // Nothing ran (a file that could not be read, Restricted Mode): the
        // summary is the whole message.
        el("main").replaceChildren();
      }
    } else if (data.type === "exported" || data.type === "counted") {
      const t = tabs.find((x) => x.entry.id === data.id);
      if (data.type === "counted" && t && typeof data.total === "number") {
        t.entry.result.more = false;
        t.entry.result.total = data.total;
        if (tabs[active] === t) status(t);
        return;
      }
      if (t) t.note = data.message;
      const note = document.querySelector(`[data-export-note="${data.id}"]`);
      if (note) note.textContent = data.message;
    }
  });

  // --- the grid -----------------------------------------------------------
  function grid(r) {
    const cols = r.columns;
    const types = r.types || [];
    const data = r.data; // data[c][row] : string | null
    const num = r.num || [];
    // `total`: the rows loaded. `n`: the rows in view, fewer while a filter
    // is on. Everything that draws or moves works in view rows; at() maps a
    // view row to its data row through the filter and the sort.
    const total = data.length ? data[0].length : 0;
    let n = total;

    const root = $("div", "grid");
    root.tabIndex = 0;
    root.setAttribute("role", "grid");
    root.setAttribute("aria-label", `${total} rows, ${cols.length} columns`);
    const viewport = $("div", "viewport");
    const canvas = $("canvas");
    const sizer = $("div", "sizer");
    viewport.append(canvas, sizer);
    root.append(viewport);
    const ctx = canvas.getContext("2d", { alpha: false });

    // Row-number column wide enough for the last row number.
    ctx.font = theme.small;
    const NUMW = Math.ceil(ctx.measureText(String(total)).width) + PAD * 2 + 4;

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
      for (let k = 0, m = Math.min(total, sample); k < m; k++) {
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
    // The grid fills whatever holds it; the page lays that out.

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

      // Grid lines: hairlines between rows and columns, half strength, so
      // the data is what the eye lands on.
      ctx.strokeStyle = t.line;
      ctx.globalAlpha = 0.5;
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
      ctx.globalAlpha = 1;

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

    // Rows the filter keeps (data rows, in order), or null for all of them.
    let kept = null;

    function sortBy(c) {
      const dir = sort.col === c ? (sort.dir === 1 ? -1 : sort.dir === -1 ? 0 : 1) : 1;
      sort = { col: dir ? c : -1, dir };
      applySort();
      repaint();
    }

    function applySort() {
      const { col: c, dir } = sort;
      if (!dir) {
        order = kept;
      } else {
        const col = data[c];
        const keys = num[c] ? Float64Array.from(col, (s) => (s === null ? NaN : Number(s))) : col;
        let idx = kept;
        if (!idx) {
          idx = new Int32Array(total);
          for (let k = 0; k < total; k++) idx[k] = k;
        }
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
    }

    /**
     * Keep only the rows `match(text)` accepts in column `c`, or in any column
     * when `c` is -1; null clears the filter. NULL is matched as the text
     * "NULL". Returns how many rows are kept.
     */
    function filter(match, c) {
      if (!match) {
        kept = null;
      } else {
        const colsToTest = c >= 0 ? [c] : cols.map((_, i) => i);
        const out = [];
        for (let k = 0; k < total; k++) {
          for (const ci of colsToTest) {
            const v = data[ci][k];
            if (match(v === null ? "NULL" : v)) {
              out.push(k);
              break;
            }
          }
        }
        kept = Int32Array.from(out);
      }
      n = kept ? kept.length : total;
      applySort();
      sel = null;
      layout();
      viewport.scrollTop = 0;
      repaint(true);
      return n;
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
            widths[edge] = fit(edge, total);
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

    root.filter = filter;
    root.focusGrid = () => root.focus();
    const g = {
      repaint: (resize) => repaint(resize),
      dispose: () => {
        ro.disconnect();
        grids.delete(g);
      },
    };
    grids.add(g);
    root.repaint = g.repaint;
    root.dispose = g.dispose;
    requestAnimationFrame(() => repaint(true));
    return root;
  }

  // Loaded (first time, or again after the view was moved): the host
  // replays whatever is on screen now.
  vscode.postMessage({ type: "ready" });
})();
