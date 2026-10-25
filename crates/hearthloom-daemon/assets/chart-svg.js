// chart-svg.js — the ONE chart renderer for Ask answers. It returns
// an SVG *string*
// from a JSON spec, dependency-free: the handful of scales our seven
// chart types need is plain math, so nothing new is vendored into the
// binary. Colors and fonts come from CSS custom properties (see the
// "charts" section of app.css), so every chart follows the embers dual
// theme automatically — never hardcode a color here.
//
// Spec (emitted by the RLM as a ```chart fenced JSON block):
//   { "type": "bar|line|area|pie|donut|scatter|heatmap",
//     "title": "…",
//     "data": [ {…}, … ],
//     // cartesian: xKey + series [{key, label?}] (or yKey shorthand)
//     // pie/donut: nameKey + valueKey
//     // scatter:   xKey + yKey (+ nameKey for point labels)
//     // heatmap:   xKey + yKey + valueKey
//   }
(function (root) {
  const SERIES = 8; // --chart-1 … --chart-8 in app.css
  const esc = (s) => String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;")
    .replace(/>/g, "&gt;").replace(/"/g, "&quot;");
  const num = (v) => { const x = Number(v); return Number.isFinite(x) ? x : 0; };
  const fmt = (v) => {
    const x = Number(v);
    // Chart values come from model output, which can echo text it read
    // (mail, web pages): a non-number is markup-escaped like any label.
    if (!Number.isFinite(x)) return esc(String(v));
    return Math.abs(x) >= 1000 ? x.toLocaleString(undefined, { maximumFractionDigits: 0 })
      : x.toLocaleString(undefined, { maximumFractionDigits: 2 });
  };
  const trunc = (s, n) => { s = String(s); return s.length > n ? s.slice(0, n - 1) + "…" : s; };
  const color = (i) => `var(--chart-${(i % SERIES) + 1})`;

  /* — tiny scales — */
  function bandScale(domain, size, padding) {
    const step = size / Math.max(domain.length, 1);
    const bw = step * (1 - padding);
    const at = new Map(domain.map((d, i) => [d, i * step + (step - bw) / 2]));
    return { at: (d) => at.get(d) ?? 0, bandwidth: bw, step };
  }
  function pointScale(domain, size) {
    const step = domain.length > 1 ? size / (domain.length - 1) : 0;
    const at = new Map(domain.map((d, i) => [d, domain.length > 1 ? i * step : size / 2]));
    return { at: (d) => at.get(d) ?? 0 };
  }
  function linearScale(max, size) {
    // "nice" ceiling: 1/2/2.5/5 × 10^k just above max.
    const raw = max > 0 ? max : 1;
    const mag = Math.pow(10, Math.floor(Math.log10(raw)));
    const top = [1, 2, 2.5, 5, 10].map((m) => m * mag).find((m) => m >= raw) || raw;
    const ticks = [0, 0.25, 0.5, 0.75, 1].map((f) => f * top);
    return { to: (v) => size - (v / top) * size, top, ticks };
  }

  /* — shared chrome — */
  function axes(x, y, cats, width, height) {
    let s = "<g>";
    for (const t of y.ticks) {
      const yy = y.to(t);
      s += `<line x1="0" y1="${yy}" x2="${width}" y2="${yy}" class="chart-grid"/>`;
      s += `<text x="-8" y="${yy + 3}" text-anchor="end" class="chart-tick">${fmt(t)}</text>`;
    }
    s += `<line x1="0" y1="${height}" x2="${width}" y2="${height}" class="chart-axis"/>`;
    // Thin x labels when crowded.
    const every = Math.ceil(cats.length / Math.max(Math.floor(width / 58), 1));
    cats.forEach((c, i) => {
      if (i % every) return;
      const cx = x.bandwidth ? x.at(c) + x.bandwidth / 2 : x.at(c);
      s += `<text x="${cx}" y="${height + 15}" text-anchor="middle" class="chart-tick">${esc(trunc(c, 12))}</text>`;
    });
    return s + "</g>";
  }
  function legend(series, width) {
    if (series.length < 2) return "";
    let s = `<g class="chart-legend" transform="translate(0,-6)">`;
    let xo = 0;
    series.forEach((sr, i) => {
      const label = sr.label || sr.key;
      s += `<circle cx="${xo + 4}" cy="-3" r="4" fill="${color(i)}"/>`;
      s += `<text x="${xo + 12}" y="0" class="chart-tick">${esc(trunc(label, 16))}</text>`;
      xo += 20 + Math.min(label.length, 16) * 6.2;
    });
    return s + "</g>";
  }
  // ── treemap: nameKey + valueKey — share of a whole as nested area
  // (squarified row packing; labels only where they fit) ──
  function treemapSVG(spec, data, title) {
    const W = 640, H = 440, top = title ? 30 : 6;
    const nameKey = spec.nameKey || "name", valueKey = spec.valueKey || "value";
    const items = data
      .map((d) => ({ name: String(d[nameKey]), v: num(d[valueKey]) }))
      .filter((i) => i.v > 0)
      .sort((a, b) => b.v - a.v);
    if (!items.length) return "";
    const total = items.reduce((a, i) => a + i.v, 0);
    let inner = "";
    let x = 0, y = top, w = W, h = H - top, idx = 0;
    let rest = items.slice();
    while (rest.length) {
      // take a row along the short side, greedily keeping cells squarish
      const horiz = w >= h;
      const side = horiz ? h : w;
      const restTotal = rest.reduce((a, i) => a + i.v, 0);
      const area = (v) => (v / restTotal) * w * h;
      let row = [rest[0]], rowArea = area(rest[0].v);
      const worst = (rowArr, ra) => {
        const thick = ra / side;
        return Math.max(...rowArr.map((i) => {
          const len = area(i.v) / thick;
          return Math.max(len / thick, thick / len);
        }));
      };
      while (rest.length > row.length) {
        const nxt = rest[row.length];
        const tryRow = row.concat(nxt), tryArea = rowArea + area(nxt.v);
        if (worst(tryRow, tryArea) <= worst(row, rowArea)) {
          row = tryRow; rowArea = tryArea;
        } else break;
      }
      const thick = rowArea / side;
      let off = 0;
      for (const i of row) {
        const len = area(i.v) / thick;
        const [rx, ry, rw, rh] = horiz
          ? [x, y + off, thick, len]
          : [x + off, y, len, thick];
        const pct = Math.round((i.v / total) * 100);
        inner += `<rect x="${rx.toFixed(1)}" y="${ry.toFixed(1)}" width="${Math.max(rw - 2, 1).toFixed(1)}" height="${Math.max(rh - 2, 1).toFixed(1)}" rx="3" fill="${color(idx)}" fill-opacity="0.85"><title>${esc(i.name)}: ${fmt(i.v)} (${pct}%)</title></rect>`;
        // label: size the font to the tile, truncate with an ellipsis
        // to fit the width; only skip when even 2 chars won't fit (the
        // instant tooltip still covers those tiles).
        const pad = 5;
        const fs = Math.max(8, Math.min(13, Math.round(rh / 2.6)));
        const maxChars = Math.floor((rw - pad * 2) / (fs * 0.6));
        if (rw > 22 && rh > 13 && maxChars >= 2) {
          let label = i.name;
          if (label.length > maxChars) label = label.slice(0, Math.max(1, maxChars - 1)) + "\u2026";
          inner += `<text x="${(rx + pad).toFixed(1)}" y="${(ry + pad + fs * 0.85).toFixed(1)}" class="chart-tick" style="font-size:${fs}px">${esc(label)}</text>`;
          if (rh > fs * 2 + 8 && rw > 34) {
            inner += `<text x="${(rx + pad).toFixed(1)}" y="${(ry + pad + fs * 2 + 1).toFixed(1)}" class="chart-tick" style="font-size:${Math.max(8, fs - 1)}px" opacity="0.7">${pct}%</text>`;
          }
        }
        off += len;
        idx += 1;
      }
      if (horiz) { x += thick; w -= thick; } else { y += thick; h -= thick; }
      rest = rest.slice(row.length);
    }
    return wrap(W, H, title, inner);
  }

  function wrap(w, h, title, body) {
    const t = title
      ? `<text x="${w / 2}" y="17" text-anchor="middle" class="chart-title-text">${esc(title)}</text>`
      : "";
    return `<svg xmlns="http://www.w3.org/2000/svg" width="${w}" height="${h}" viewBox="0 0 ${w} ${h}" class="hl-chart" role="img" aria-label="${esc(title || "chart")}">${t}${body}</svg>`;
  }

  function buildChartSVG(spec, opts) {
    try {
      return build(spec, opts || {});
    } catch (err) {
      return `<div class="chart-error">chart failed to render: ${esc(err.message)}</div>`;
    }
  }

  function build(spec, opts) {
    if (!spec || !spec.type || !Array.isArray(spec.data) || spec.data.length === 0) return "";
    const data = spec.data;
    const title = spec.title ? String(spec.title) : "";
    if (spec.type === "treemap") return treemapSVG(spec, data, title);
    if (spec.type === "pie" || spec.type === "donut") return pieSVG(spec, data, title);
    if (spec.type === "heatmap") return heatmapSVG(spec, data, title);
    if (spec.type === "scatter") return scatterSVG(spec, data, title);
    if (spec.type === "map") return mapSVG(spec, data, title);

    // ── cartesian: bar / line / area ──
    const W = opts.width || 560, H = opts.height || 300;
    const m = { top: title ? 44 : 26, right: 14, bottom: 36, left: 56 };
    const width = W - m.left - m.right, height = H - m.top - m.bottom;
    const xKey = spec.xKey || "name";
    const series = spec.series || [{ key: spec.yKey || "value" }];
    const cats = data.map((d) => String(d[xKey]));
    const maxV = Math.max(...data.flatMap((d) => series.map((s) => num(d[s.key]))), 0);
    const y = linearScale(maxV, height);
    let inner = legend(series, width);

    if (spec.type === "bar") {
      const x = bandScale(cats, width, 0.25);
      inner += axes(x, y, cats, width, height);
      series.forEach((s, si) => {
        const bw = x.bandwidth / series.length;
        data.forEach((d) => {
          const v = num(d[s.key]);
          const by = y.to(v);
          inner += `<rect x="${(x.at(String(d[xKey])) + bw * si).toFixed(1)}" y="${by.toFixed(1)}" width="${Math.max(bw - 1, 1).toFixed(1)}" height="${Math.max(height - by, 0).toFixed(1)}" rx="2" fill="${color(si)}"><title>${esc(d[xKey])}: ${fmt(v)}</title></rect>`;
        });
      });
    } else { // line / area
      const x = pointScale(cats, width);
      inner += axes(x, y, cats, width, height);
      series.forEach((s, si) => {
        const pts = data.map((d) => `${x.at(String(d[xKey])).toFixed(1)},${y.to(num(d[s.key])).toFixed(1)}`);
        if (spec.type === "area") {
          inner += `<polygon points="0,${height} ${pts.join(" ")} ${width},${height}" fill="${color(si)}" opacity="0.16"/>`;
        }
        inner += `<polyline points="${pts.join(" ")}" fill="none" stroke="${color(si)}" stroke-width="2" stroke-linejoin="round"/>`;
        if (data.length <= 30) {
          data.forEach((d) => {
            inner += `<circle cx="${x.at(String(d[xKey])).toFixed(1)}" cy="${y.to(num(d[s.key])).toFixed(1)}" r="2.6" fill="${color(si)}"><title>${esc(d[xKey])}: ${fmt(d[s.key])}</title></circle>`;
          });
        }
      });
    }
    return wrap(W, H, title, `<g transform="translate(${m.left},${m.top})">${inner}</g>`);
  }

  // Pie/donut with leader lines: name+value OUTSIDE,
  // percentage INSIDE (skipped for slivers), total in a donut's hole.
  function pieSVG(spec, data, title) {
    const nameKey = spec.nameKey || "name";
    const valueKey = spec.valueKey || "value";
    const isDonut = spec.type === "donut";
    const W = 560, H = 320;
    const cx = W / 2, cy = H / 2 + (title ? 12 : 0);
    const r = Math.min(W - 240, H - (title ? 70 : 40)) / 2;
    const r0 = isDonut ? r * 0.6 : 0;
    const total = data.reduce((a, d) => a + num(d[valueKey]), 0) || 1;

    let a0 = -Math.PI / 2;
    let body = `<g transform="translate(${cx},${cy})">`;
    let labels = "";
    data.forEach((d, i) => {
      const v = num(d[valueKey]);
      const a1 = a0 + (v / total) * Math.PI * 2;
      const large = a1 - a0 > Math.PI ? 1 : 0;
      const p = (a, rad) => `${(Math.cos(a) * rad).toFixed(2)},${(Math.sin(a) * rad).toFixed(2)}`;
      body += isDonut
        ? `<path d="M${p(a0, r)} A${r},${r} 0 ${large} 1 ${p(a1, r)} L${p(a1, r0)} A${r0},${r0} 0 ${large} 0 ${p(a0, r0)} Z" fill="${color(i)}" class="chart-slice"><title>${esc(d[nameKey])}: ${fmt(v)}</title></path>`
        : `<path d="M0,0 L${p(a0, r)} A${r},${r} 0 ${large} 1 ${p(a1, r)} Z" fill="${color(i)}" class="chart-slice"><title>${esc(d[nameKey])}: ${fmt(v)}</title></path>`;
      const mid = (a0 + a1) / 2;
      const side = Math.cos(mid) >= 0 ? 1 : -1;
      const ex = Math.cos(mid) * r, ey = Math.sin(mid) * r;
      const bx = side * (r + 16), ax = side * (r + 24);
      labels += `<polyline points="${ex.toFixed(1)},${ey.toFixed(1)} ${bx},${ey.toFixed(1)} ${ax},${ey.toFixed(1)}" fill="none" class="chart-leader"/>`;
      labels += `<text x="${ax + side * 4}" y="${(ey + 3).toFixed(1)}" text-anchor="${side > 0 ? "start" : "end"}" class="chart-label">${esc(trunc(d[nameKey], 18))} <tspan class="chart-tick">${fmt(v)}</tspan></text>`;
      const pct = (v / total) * 100;
      if (pct >= 5) {
        const lr = (r + r0) / 2;
        labels += `<text x="${(Math.cos(mid) * lr).toFixed(1)}" y="${(Math.sin(mid) * lr + 3).toFixed(1)}" text-anchor="middle" class="chart-pct">${pct.toFixed(0)}%</text>`;
      }
      a0 = a1;
    });
    if (isDonut) {
      body += `<text text-anchor="middle" dy="-0.1em" class="chart-total">${fmt(total)}</text>`;
      body += `<text text-anchor="middle" dy="1.5em" class="chart-tick">total</text>`;
    }
    body += labels + "</g>";
    return wrap(W, H, title, body);
  }

  function scatterSVG(spec, data, title) {
    const W = 560, H = 320;
    const m = { top: title ? 44 : 26, right: 14, bottom: 40, left: 56 };
    const width = W - m.left - m.right, height = H - m.top - m.bottom;
    const xKey = spec.xKey || "x", yKey = spec.yKey || "y";
    const maxX = Math.max(...data.map((d) => num(d[xKey])), 0);
    const maxY = Math.max(...data.map((d) => num(d[yKey])), 0);
    const x = linearScale(maxX, width), y = linearScale(maxY, height);
    let inner = "<g>";
    for (const t of y.ticks) {
      inner += `<line x1="0" y1="${y.to(t)}" x2="${width}" y2="${y.to(t)}" class="chart-grid"/>`;
      inner += `<text x="-8" y="${y.to(t) + 3}" text-anchor="end" class="chart-tick">${fmt(t)}</text>`;
    }
    for (const t of x.ticks) {
      inner += `<text x="${width - x.to(t)}" y="${height + 15}" text-anchor="middle" class="chart-tick">${fmt(t)}</text>`;
    }
    inner += `<line x1="0" y1="${height}" x2="${width}" y2="${height}" class="chart-axis"/>`;
    data.forEach((d) => {
      const label = spec.nameKey ? `${d[spec.nameKey]}: ` : "";
      inner += `<circle cx="${(width - x.to(num(d[xKey]))).toFixed(1)}" cy="${y.to(num(d[yKey])).toFixed(1)}" r="4" fill="${color(0)}" opacity="0.75"><title>${esc(label)}${fmt(d[xKey])}, ${fmt(d[yKey])}</title></circle>`;
    });
    if (spec.xLabel) inner += `<text x="${width / 2}" y="${height + 30}" text-anchor="middle" class="chart-tick">${esc(spec.xLabel)}</text>`;
    if (spec.yLabel) inner += `<text transform="rotate(-90)" x="${-height / 2}" y="-42" text-anchor="middle" class="chart-tick">${esc(spec.yLabel)}</text>`;
    return wrap(W, H, title, `<g transform="translate(${m.left},${m.top})">${inner}</g>`);
  }

  // Single-hue heatmap: intensity = opacity of the theme's heat color,
  // so both themes stay legible without a computed color ramp.
  function heatmapSVG(spec, data, title) {
    const xKey = spec.xKey || "x", yKey = spec.yKey || "y", vKey = spec.valueKey || "value";
    const xs = [...new Set(data.map((d) => String(d[xKey])))];
    const ys = [...new Set(data.map((d) => String(d[yKey])))];
    const W = 560;
    const cellH = Math.max(Math.min(26, 220 / ys.length), 14);
    const m = { top: title ? 44 : 26, right: 14, bottom: 34, left: 96 };
    const height = cellH * ys.length;
    const H = height + m.top + m.bottom;
    const width = W - m.left - m.right;
    const x = bandScale(xs, width, 0.06);
    const yAt = (v) => ys.indexOf(v) * cellH;
    const maxV = Math.max(...data.map((d) => num(d[vKey])), 0) || 1;
    let inner = "";
    data.forEach((d) => {
      const v = num(d[vKey]);
      const op = v <= 0 ? 0.04 : 0.12 + 0.88 * (v / maxV);
      inner += `<rect x="${x.at(String(d[xKey])).toFixed(1)}" y="${yAt(String(d[yKey]))}" width="${x.bandwidth.toFixed(1)}" height="${cellH - 2}" rx="2" fill="var(--chart-heat)" fill-opacity="${op.toFixed(2)}"><title>${esc(d[yKey])} · ${esc(d[xKey])}: ${fmt(v)}</title></rect>`;
    });
    ys.forEach((v) => {
      inner += `<text x="-8" y="${yAt(v) + cellH / 2 + 3}" text-anchor="end" class="chart-tick">${esc(trunc(v, 14))}</text>`;
    });
    const every = Math.ceil(xs.length / Math.max(Math.floor(width / 52), 1));
    xs.forEach((c, i) => {
      if (i % every) return;
      inner += `<text x="${(x.at(c) + x.bandwidth / 2).toFixed(1)}" y="${height + 14}" text-anchor="middle" class="chart-tick">${esc(trunc(c, 10))}</text>`;
    });
    return wrap(W, H, title, `<g transform="translate(${m.left},${m.top})">${inner}</g>`);
  }

  // Inline map: points (and an optional journey line) over the vendored
  // world geometry (set by app.js as window.__worldLand). Auto-fits the
  // viewport to the data, so a regional trip zooms in and a world tour
  // zooms out.
  function mapSVG(spec, data, title) {
    const worldData = root.__worldLand;
    if (!worldData) return `<div class="chart-error">map geometry not loaded yet</div>`;
    const latK = spec.latKey || "lat", lonK = spec.lonKey || "lon";
    const pts = data
      .map((d) => ({ lat: num(d[latK]), lon: num(d[lonK]), label: spec.labelKey ? d[spec.labelKey] : d.label, value: d.value }))
      .filter((p) => p.lat >= -90 && p.lat <= 90 && p.lon >= -180 && p.lon <= 180);
    if (!pts.length) return "";
    const W = 560, H = 340, headroom = title ? 26 : 6;
    // fit: bbox + padding, min span, aspect-corrected
    let minLat = Math.min(...pts.map((p) => p.lat)), maxLat = Math.max(...pts.map((p) => p.lat));
    let minLon = Math.min(...pts.map((p) => p.lon)), maxLon = Math.max(...pts.map((p) => p.lon));
    const padLat = Math.max((maxLat - minLat) * 0.25, 4);
    const padLon = Math.max((maxLon - minLon) * 0.25, 6);
    minLat -= padLat; maxLat += padLat; minLon -= padLon; maxLon += padLon;
    const mapH = H - headroom - 6;
    const scale = Math.min(W / (maxLon - minLon), mapH / (maxLat - minLat));
    const cx = (minLon + maxLon) / 2, cy = (minLat + maxLat) / 2;
    const X = (lon) => W / 2 + (lon - cx) * scale;
    const Y = (lat) => headroom + mapH / 2 - (lat - cy) * scale;

    let land = "";
    for (const poly of worldData.polys) {
      let path = `M${X(poly[0][0]).toFixed(1)},${Y(poly[0][1]).toFixed(1)}`;
      for (let i = 1; i < poly.length; i++) path += `L${X(poly[i][0]).toFixed(1)},${Y(poly[i][1]).toFixed(1)}`;
      land += `<path d="${path}Z" class="chart-map-land"/>`;
    }
    let marks = "";
    if (spec.connect) {
      const line = pts.map((p) => `${X(p.lon).toFixed(1)},${Y(p.lat).toFixed(1)}`).join(" ");
      marks += `<polyline points="${line}" fill="none" stroke="${color(2)}" stroke-width="1.4" stroke-dasharray="4 3" opacity="0.8"/>`;
    }
    const maxV = Math.max(...pts.map((p) => num(p.value) || 1), 1);
    pts.forEach((p) => {
      const r = 3 + 5 * Math.sqrt((num(p.value) || 1) / maxV);
      marks += `<circle cx="${X(p.lon).toFixed(1)}" cy="${Y(p.lat).toFixed(1)}" r="${r.toFixed(1)}" fill="${color(0)}" opacity="0.85" class="chart-map-pt"><title>${esc(p.label || "")} ${p.value != null ? fmt(p.value) : ""}</title></circle>`;
      if (p.label && pts.length <= 12) {
        marks += `<text x="${(X(p.lon) + r + 3).toFixed(1)}" y="${(Y(p.lat) + 3).toFixed(1)}" class="chart-label">${esc(trunc(p.label, 18))}</text>`;
      }
    });
    return wrap(W, H, title, `<clipPath id="mapclip"><rect x="0" y="${headroom}" width="${W}" height="${mapH + 6}"/></clipPath><g clip-path="url(#mapclip)">${land}${marks}</g>`);
  }

  root.buildChartSVG = buildChartSVG;
})(typeof globalThis !== "undefined" ? globalThis : this);
