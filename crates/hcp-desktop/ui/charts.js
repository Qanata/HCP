// Charts for the HCP dashboard, drawn as SVG with DOM calls. Every label comes from
// request data (agent names, summaries), so text is only ever set with textContent.

const SVG = "http://www.w3.org/2000/svg";

export function el(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [name, value] of Object.entries(attrs)) {
    if (value === undefined || value === null || value === false) continue;
    if (name === "class") node.className = value;
    else if (name === "text") node.textContent = value;
    else if (name.startsWith("on")) node.addEventListener(name.slice(2), value);
    else node.setAttribute(name, value === true ? "" : value);
  }
  for (const child of children.flat()) {
    if (child === null || child === undefined || child === false) continue;
    node.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
  return node;
}

function svg(tag, attrs = {}) {
  const node = document.createElementNS(SVG, tag);
  for (const [name, value] of Object.entries(attrs)) node.setAttribute(name, value);
  return node;
}

export function compact(n) {
  if (n === null || n === undefined) return "–";
  if (Math.abs(n) >= 10000) return new Intl.NumberFormat(undefined, { notation: "compact", maximumFractionDigits: 1 }).format(n);
  return new Intl.NumberFormat().format(n);
}

/** Clean axis ticks from 0 to at least `max`. */
function ticks(max, count = 4) {
  if (max <= 0) return [0, 1];
  const raw = max / count;
  const magnitude = 10 ** Math.floor(Math.log10(raw));
  const step = [1, 2, 2.5, 5, 10].map((m) => m * magnitude).find((s) => s >= raw);
  const whole = Math.max(1, Math.round(step));
  const top = Math.ceil(max / whole) * whole;
  const out = [];
  for (let v = 0; v <= top; v += whole) out.push(v);
  return out;
}

// ── Tooltip ────────────────────────────────────────────────────────────────

const tooltip = el("div", { class: "tooltip", role: "tooltip", hidden: true });
document.body.append(tooltip);

/** rows: [{ value, name, color }] — values lead, names follow, keyed by a short line. */
export function showTooltip(event, title, rows) {
  tooltip.replaceChildren(
    el("div", { class: "title", text: title }),
    ...rows.map((row) =>
      el(
        "div",
        { class: "line" },
        el("span", { class: "key", style: `background:${row.color}` }),
        el("span", { class: "v", text: compact(row.value) }),
        el("span", { class: "n", text: row.name }),
      ),
    ),
  );
  tooltip.hidden = false;
  const rect = event.target.getBoundingClientRect?.();
  const x = event.clientX ?? (rect ? rect.right : 0);
  const y = event.clientY ?? (rect ? rect.top : 0);
  const { width, height } = tooltip.getBoundingClientRect();
  const left = x + 14 + width > window.innerWidth ? x - width - 14 : x + 14;
  const top = Math.min(window.innerHeight - height - 8, Math.max(8, y - height / 2));
  tooltip.style.left = `${left}px`;
  tooltip.style.top = `${top}px`;
}

export function hideTooltip() {
  tooltip.hidden = true;
}

/** Wires hover and keyboard focus to the same tooltip. */
function hoverable(node, title, rows) {
  node.setAttribute("tabindex", "0");
  node.addEventListener("pointermove", (e) => showTooltip(e, title, rows));
  node.addEventListener("pointerleave", hideTooltip);
  node.addEventListener("focus", (e) => showTooltip(e, title, rows));
  node.addEventListener("blur", hideTooltip);
}

/** A rectangle with 4px rounded corners on its top end only. */
function topRounded(x, y, w, h, r) {
  r = Math.min(r, w / 2, h);
  return `M${x},${y + h} V${y + r} Q${x},${y} ${x + r},${y} H${x + w - r} Q${x + w},${y} ${x + w},${y + r} V${y + h} Z`;
}

/** A rectangle with 4px rounded corners on its right end only. */
function rightRounded(x, y, w, h, r) {
  r = Math.min(r, h / 2, w);
  return `M${x},${y} H${x + w - r} Q${x + w},${y} ${x + w},${y + r} V${y + h - r} Q${x + w},${y + h} ${x + w - r},${y + h} H${x} Z`;
}

// ── Stacked columns: one column per day, segments in fixed series order ────

/**
 * series: [{ key, name, color }] bottom to top (fixed palette order, never re-ranked).
 * days:   [{ label, long, values: { [key]: number } }]
 */
export function stackedColumns(container, { series, days, height = 220 }) {
  container.replaceChildren();
  const width = Math.max(320, container.clientWidth);
  const left = 36, right = 8, top = 8, bottom = 26;
  const plotW = width - left - right, plotH = height - top - bottom;
  const totals = days.map((d) => series.reduce((sum, s) => sum + (d.values[s.key] || 0), 0));
  const axis = ticks(Math.max(...totals, 0));
  const max = axis[axis.length - 1];
  const y = (v) => top + plotH - (v / max) * plotH;
  const band = plotW / Math.max(1, days.length);
  const barW = Math.min(24, Math.max(2, band * 0.62));

  const root = svg("svg", { viewBox: `0 0 ${width} ${height}`, height, role: "img" });
  root.setAttribute("aria-label", "Requests per day, stacked by outcome");

  for (const t of axis) {
    root.append(svg("line", { class: t === 0 ? "baseline" : "gridline", x1: left, x2: width - right, y1: y(t), y2: y(t) }));
    const label = svg("text", { class: "tick", x: left - 8, y: y(t) + 4, "text-anchor": "end" });
    label.textContent = compact(t);
    root.append(label);
  }

  const every = Math.ceil(days.length / Math.max(1, Math.floor(plotW / 64)));
  days.forEach((day, i) => {
    const cx = left + band * i + band / 2;
    const present = series.filter((s) => (day.values[s.key] || 0) > 0);
    let base = 0;
    present.forEach((s, index) => {
      const v = day.values[s.key];
      const y0 = y(base), y1 = y(base + v);
      // 2px surface gap between stacked segments.
      const gap = index > 0 ? 2 : 0;
      const h = Math.max(0, y0 - y1 - gap);
      if (h > 0) {
        const isTop = index === present.length - 1;
        root.append(
          isTop
            ? svg("path", { d: topRounded(cx - barW / 2, y1, barW, h, 4), fill: s.color })
            : svg("rect", { x: cx - barW / 2, y: y1, width: barW, height: h, fill: s.color }),
        );
      }
      base += v;
    });
    if (i % every === 0 || i === days.length - 1) {
      const label = svg("text", { class: "tick", x: cx, y: height - 8, "text-anchor": "middle" });
      label.textContent = day.label;
      root.append(label);
    }
    // The whole day is the hit target, bigger than any segment.
    const hit = svg("rect", { class: "band", x: left + band * i, y: top, width: band, height: plotH, rx: 4 });
    hit.setAttribute("aria-label", `${day.long}: ${totals[i]} requests`);
    hoverable(
      hit,
      `${day.long} · ${compact(totals[i])} total`,
      series.map((s) => ({ value: day.values[s.key] || 0, name: s.name, color: s.color })).reverse(),
    );
    root.append(hit);
  });

  container.append(root);
}

// ── Ranked horizontal bars: one hue, value at the tip ──────────────────────

/** rows: [{ name, value, detail? }] largest first; color: one hue for every bar. */
export function rankedBars(container, { rows, color, unit = "requests" }) {
  container.replaceChildren();
  if (!rows.length) {
    container.append(el("div", { class: "empty", text: "Nothing in this period" }));
    return;
  }
  const width = Math.max(280, container.clientWidth);
  const nameW = Math.min(150, width * 0.36), valueW = 44, rowH = 30, barH = 14;
  const height = rows.length * rowH;
  const max = Math.max(...rows.map((r) => r.value), 1);
  const plotW = width - nameW - valueW;
  const root = svg("svg", { viewBox: `0 0 ${width} ${height}`, height, role: "img" });
  root.setAttribute("aria-label", `Ranked by ${unit}`);

  rows.forEach((row, i) => {
    const cy = i * rowH + rowH / 2;
    const name = svg("text", { class: "bar-name", x: 0, y: cy + 4 });
    name.textContent = row.name.length > 22 ? `${row.name.slice(0, 21)}…` : row.name;
    root.append(name);
    root.append(svg("line", { class: "baseline", x1: nameW, x2: nameW, y1: cy - barH / 2 - 3, y2: cy + barH / 2 + 3 }));
    const w = Math.max(row.value > 0 ? 3 : 0, (row.value / max) * plotW);
    if (w > 0) root.append(svg("path", { d: rightRounded(nameW, cy - barH / 2, w, barH, 4), fill: color }));
    const value = svg("text", { class: "bar-label", x: nameW + w + 6, y: cy + 4 });
    value.textContent = compact(row.value);
    root.append(value);
    const hit = svg("rect", { class: "band", x: 0, y: i * rowH, width, height: rowH, rx: 4 });
    hit.setAttribute("aria-label", `${row.name}: ${row.value} ${unit}`);
    hoverable(hit, row.name, [{ value: row.value, name: row.detail || unit, color }]);
    root.append(hit);
  });
  container.append(root);
}

// ── Legend and table view ──────────────────────────────────────────────────

export function legend(series, totals) {
  return el(
    "ul",
    { class: "legend" },
    series.map((s) =>
      el(
        "li",
        {},
        el("span", { class: "swatch", style: `background:${s.color}` }),
        el("span", { text: s.name }),
        totals ? el("span", { class: "count", text: compact(totals[s.key] || 0) }) : null,
      ),
    ),
  );
}

/** columns: [{ label, numeric? }]; rows: arrays of cell text. */
export function table(columns, rows) {
  return el(
    "table",
    {},
    el("thead", {}, el("tr", {}, columns.map((c) => el("th", { class: c.numeric ? "n" : "", text: c.label })))),
    el(
      "tbody",
      {},
      rows.map((cells) => el("tr", {}, cells.map((cell, i) => el("td", { class: columns[i].numeric ? "n" : "", text: cell })))),
    ),
  );
}
