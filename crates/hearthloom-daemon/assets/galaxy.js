// The Galaxy: the record seen whole. Sources are suns arranged on a
// ring; each sun's most-cited entities are its planets; a planet with
// evidence in several sources hangs BETWEEN them — the bridge people
// become literally central, which is the point. Co-citation draws faint
// gravity between planets; idle-computed semantic pairs cross regions
// as comets. Clicking a planet drills into the classic neighborhood
// constellation; the whole thing rides the Dial for time-lapse.
//
// Perf: one /v1/galaxy fetch (hard-capped server side), a few hundred
// force iterations run synchronously at build — no physics engine, no
// traversal, nothing per-frame but rendering.

import * as THREE from "three";
import { OrbitControls } from "three/addons/controls/OrbitControls.js";

const SOURCE_COLORS = [
  0xe8833a, 0x7fd7c4, 0xffd166, 0xc9a7eb, 0xef8354, 0x6fb3ab,
  0xd9a441, 0x8fb6e8, 0xe89ab0, 0x9ac48a, 0xb8b2a6, 0xcf8de0,
  0x76c7e0, 0xe0b276,
];
const TYPE_COLORS = { Person: 0xffb26b, Org: 0xef8354, Group: 0xc9a7eb, Project: 0xffd166 };

let frame = 0;

function glowTexture() {
  const size = 128;
  const canvas = document.createElement("canvas");
  canvas.width = canvas.height = size;
  const ctx = canvas.getContext("2d");
  const g = ctx.createRadialGradient(size / 2, size / 2, 0, size / 2, size / 2, size / 2);
  g.addColorStop(0, "rgba(255,255,255,1)");
  g.addColorStop(0.3, "rgba(255,255,255,0.8)");
  g.addColorStop(0.6, "rgba(255,255,255,0.15)");
  g.addColorStop(1, "rgba(255,255,255,0)");
  ctx.fillStyle = g;
  ctx.fillRect(0, 0, size, size);
  return new THREE.CanvasTexture(canvas);
}

function labelSprite(text, cssColor, scale) {
  const shown = text.length > 30 ? text.slice(0, 29) + "…" : text;
  const font = `500 26px "Iowan Old Style", Palatino, Georgia, serif`;
  const measure = document.createElement("canvas").getContext("2d");
  measure.font = font;
  const w = measure.measureText(shown).width + 24;
  const canvas = document.createElement("canvas");
  canvas.width = Math.ceil(w * 2);
  canvas.height = 88;
  const ctx = canvas.getContext("2d");
  ctx.scale(2, 2);
  ctx.font = font;
  ctx.fillStyle = cssColor;
  ctx.shadowColor = "rgba(0,0,0,0.85)";
  ctx.shadowBlur = 7;
  ctx.textBaseline = "middle";
  ctx.fillText(shown, 12, 22);
  const texture = new THREE.CanvasTexture(canvas);
  const sprite = new THREE.Sprite(
    new THREE.SpriteMaterial({ map: texture, transparent: true, depthWrite: false })
  );
  sprite.scale.set((w / 11) * scale, 4 * scale, 1);
  return sprite;
}

/* — layout: suns fixed on a ring; planets settle between their suns — */
function layoutGalaxy(data) {
  const suns = data.sources.map(([name, n], i) => ({
    name, n,
    color: SOURCE_COLORS[i % SOURCE_COLORS.length],
    pos: new THREE.Vector3(),
  }));
  // Notes takes a ring slot like any other sun (same plane — it IS a
  // first-class region of the map, just fed by files instead of feeds)
  const slots = suns.length + 1;
  const R = 34 + slots * 5;
  suns.forEach((s, i) => {
    const a = (i / slots) * Math.PI * 2 - Math.PI / 2;
    s.pos.set(Math.cos(a) * R, 0, Math.sin(a) * R);
  });
  const notesAngle = ((slots - 1) / slots) * Math.PI * 2 - Math.PI / 2;
  const notesSun = {
    pos: new THREE.Vector3(Math.cos(notesAngle) * R, 0, Math.sin(notesAngle) * R),
  };
  const sunByName = new Map(suns.map((s) => [s.name, s]));

  const planets = data.nodes.map((n, i) => {
    const spokes = n.spokes.filter(([src]) => sunByName.has(src));
    const total = spokes.reduce((a, [, c]) => a + c, 0) || 1;
    // seed at the evidence-weighted centroid of its suns
    const pos = new THREE.Vector3();
    for (const [src, c] of spokes) pos.addScaledVector(sunByName.get(src).pos, (c / total) * 0.72);
    pos.x += Math.sin(i * 12.9898) * 6;
    pos.z += Math.cos(i * 78.233) * 6;
    pos.y = Math.sin(i * 3.7) * 3;
    return { ...n, spokes, total, pos };
  });
  const byId = new Map(planets.map((p) => [p.id, p]));
  const springs = data.links
    .filter((l) => byId.has(l.a) && byId.has(l.b))
    .map((l) => ({ a: byId.get(l.a), b: byId.get(l.b), w: Math.min(Math.log2(1 + l.n), 4) }));

  // relaxation: sun gravity + co-citation springs + planet repulsion
  for (let iter = 0; iter < 260; iter++) {
    for (const p of planets) {
      const pull = new THREE.Vector3();
      for (const [src, c] of p.spokes) {
        pull.addScaledVector(
          sunByName.get(src).pos.clone().sub(p.pos), (c / p.total) * 0.045);
      }
      p.pos.add(pull);
      // never sit ON a sun — orbit it
      for (const s of suns) {
        const d = p.pos.clone().sub(s.pos);
        const len = Math.max(d.length(), 0.01);
        if (len < 11) p.pos.addScaledVector(d.normalize(), (11 - len) * 0.5);
      }
      p.pos.y *= 0.92;
    }
    for (const s of springs) {
      const d = s.b.pos.clone().sub(s.a.pos).multiplyScalar(0.004 * s.w);
      s.a.pos.add(d);
      s.b.pos.sub(d);
    }
    for (let i = 0; i < planets.length; i++) {
      for (let j = i + 1; j < planets.length; j++) {
        const d = planets[i].pos.clone().sub(planets[j].pos);
        const len = Math.max(d.length(), 0.1);
        if (len < 9) {
          const push = d.normalize().multiplyScalar((9 - len) * 0.22);
          planets[i].pos.add(push);
          planets[j].pos.sub(push);
        }
      }
    }
  }

  // comets: midpoint of their anchors (cited planet, else source sun)
  const comets = (data.comets || []).map((c, i) => {
    const anchor = (end) =>
      (end.entities.map((id) => byId.get(id)).find(Boolean)?.pos
        || sunByName.get(end.source)?.pos
        || new THREE.Vector3()).clone();
    const a = anchor(c.a), b = anchor(c.b);
    const mid = a.clone().add(b).multiplyScalar(0.5);
    mid.y += 6 + (i % 5) * 2; // ride above the plane — a different kind of object
    return { ...c, aPos: mid.clone().addScaledVector(a.clone().sub(b).normalize(), 2.2),
             bPos: mid.clone().addScaledVector(b.clone().sub(a).normalize(), 2.2) };
  });

  // — the Notes cluster: notes gather around their sun by shared tags
  //   and pull toward the people they @-mention (lines at render). —
  const notes = (data.notes || []).map((n, i) => {
    const pos = notesSun.pos.clone().add(new THREE.Vector3(
      Math.sin(i * 12.9898) * 12, Math.cos(i * 4.7) * 4, Math.cos(i * 78.233) * 12));
    return { ...n, pos };
  });
  for (let iter = 0; iter < 140; iter++) {
    for (const n of notes) {
      // home gravity
      n.pos.addScaledVector(notesSun.pos.clone().sub(n.pos), 0.03);
      // pull toward mentioned planets (stay airborne: damp y)
      for (const id of n.mentions) {
        const p = byId.get(id);
        if (p) {
          const d = p.pos.clone().sub(n.pos).multiplyScalar(0.012);
          d.y *= 0.25;
          n.pos.add(d);
        }
      }
    }
    // shared tags attract — the semantic clusters
    for (let i = 0; i < notes.length; i++) {
      for (let j = i + 1; j < notes.length; j++) {
        const shared = notes[i].tags.filter((t) => notes[j].tags.includes(t)).length;
        if (shared > 0) {
          const d = notes[j].pos.clone().sub(notes[i].pos).multiplyScalar(0.01 * Math.min(shared, 3));
          notes[i].pos.add(d);
          notes[j].pos.sub(d);
        }
        const gap = notes[i].pos.clone().sub(notes[j].pos);
        const len = Math.max(gap.length(), 0.1);
        if (len < 4.5) {
          const push = gap.normalize().multiplyScalar((4.5 - len) * 0.3);
          notes[i].pos.add(push);
          notes[j].pos.sub(push);
        }
      }
    }
  }
  return { suns, planets, springs, comets, notesSun, notes, byId };
}

// Interaction model (Jon 2026-07-10): click a SUN → zoom into its
// system in place (that source's bodies, cleanly ranked orbits);
// click a PLANET → a Person pops their card (stay in context),
// anything else goes to Memories filtered to them; comets → that day.
// Background click / Escape ascends back to the galaxy.
export async function mount(canvas, { onPerson, onMemories, onBrowseSource, onDay, onNote, onNotesSun, hud, api, esc }) {
  cancelAnimationFrame(frame);
  const parent = canvas.parentElement;
  const width = parent.clientWidth;
  const height = parent.clientHeight;
  const night = document.documentElement.dataset.theme !== "daylight";
  const bg = night ? 0x14110e : 0xf2ebdd;

  const renderer = new THREE.WebGLRenderer({ canvas, antialias: true });
  renderer.setPixelRatio(Math.min(devicePixelRatio, 2));
  renderer.setSize(width, height, false);
  renderer.setClearColor(bg, 1);
  const scene = new THREE.Scene();
  scene.fog = new THREE.FogExp2(bg, 0.0045);
  const camera = new THREE.PerspectiveCamera(48, width / height, 0.1, 900);
  camera.position.set(0, 150, 210);
  // one flight machine: glide the camera (and optionally the orbit
  // target), then fire onArrive — descents and ascents are continuous
  // camera moves, never scene jumps.
  const entry = { active: true, to: new THREE.Vector3(0, 92, 120), onArrive: null };
  let glideTarget = null;
  const controls = new OrbitControls(camera, canvas);
  controls.enableDamping = true;
  controls.dampingFactor = 0.06;
  controls.minDistance = 30;
  controls.maxDistance = 420;
  controls.autoRotate = true;
  controls.autoRotateSpeed = 0.22;
  canvas.addEventListener("pointerdown", () => (controls.autoRotate = false), { once: true });

  // ambient starfield, same sky as the neighborhood view
  {
    const count = 1200;
    const positions = new Float32Array(count * 3);
    for (let i = 0; i < count; i++) {
      const r = 260 + Math.random() * 320;
      const theta = Math.random() * Math.PI * 2;
      const phi = Math.acos(2 * Math.random() - 1);
      positions[i * 3] = r * Math.sin(phi) * Math.cos(theta);
      positions[i * 3 + 1] = r * Math.cos(phi) * 0.55;
      positions[i * 3 + 2] = r * Math.sin(phi) * Math.sin(theta);
    }
    const geometry = new THREE.BufferGeometry();
    geometry.setAttribute("position", new THREE.BufferAttribute(positions, 3));
    scene.add(new THREE.Points(geometry, new THREE.PointsMaterial({
      size: 0.8, color: night ? 0x6f6455 : 0xa89a86,
      transparent: true, opacity: 0.5, sizeAttenuation: true,
    })));
  }

  const glow = glowTexture();
  let group = null;           // everything rebuilt per window/mode
  let pickables = [];         // meshes with .userData for raycast
  let mode = "galaxy";        // galaxy | system
  let currentWindow = [undefined, undefined];
  let lastLayout = null;      // galaxy layout (for sun positions)
  let lastSunPos = null;      // where we descended from (for the ride home)

  function fmtSpokes(p) {
    return p.spokes
      .slice()
      .sort((a, b) => b[1] - a[1])
      .map(([s, n]) => `${esc(s)} ${n}`)
      .join(" · ");
  }

  async function fetchGalaxy(perSource) {
    const [from, to] = currentWindow;
    const q = `${from ? `from=${from}&` : ""}${to ? `to=${to}&` : ""}`;
    return api(`/v1/galaxy?${q}per_source=${perSource}`);
  }

  async function build(from, to, opts = {}) {
    mode = "galaxy";
    currentWindow = [from, to];
    if (hud) hud.innerHTML = `<div class="meta">charting the whole record…</div>`;
    let data;
    try {
      data = await fetchGalaxy(9);
    } catch (err) {
      if (hud) hud.innerHTML = `<div class="meta">${esc(err.message)}</div>`;
      return;
    }
    const g = layoutGalaxy(data);
    lastLayout = g;
    if (group) scene.remove(group);
    group = new THREE.Group();
    pickables = [];

    // spokes first (faintest layer): each planet to its suns
    {
      const positions = [];
      const colors = [];
      for (const p of g.planets) {
        for (const [src, c] of p.spokes) {
          const sun = g.suns.find((s) => s.name === src);
          if (!sun) continue;
          const alpha = Math.min(0.06 + (c / p.total) * 0.18, 0.25);
          const color = new THREE.Color(sun.color).multiplyScalar(alpha);
          positions.push(p.pos.x, p.pos.y, p.pos.z, sun.pos.x, sun.pos.y, sun.pos.z);
          colors.push(color.r, color.g, color.b, color.r, color.g, color.b);
        }
      }
      const geometry = new THREE.BufferGeometry();
      geometry.setAttribute("position", new THREE.Float32BufferAttribute(positions, 3));
      geometry.setAttribute("color", new THREE.Float32BufferAttribute(colors, 3));
      group.add(new THREE.LineSegments(geometry, new THREE.LineBasicMaterial({
        vertexColors: true, transparent: true, opacity: 0.85, blending: THREE.AdditiveBlending,
      })));
    }
    // co-citation gravity between planets
    {
      const positions = [];
      for (const s of g.springs) {
        positions.push(s.a.pos.x, s.a.pos.y, s.a.pos.z, s.b.pos.x, s.b.pos.y, s.b.pos.z);
      }
      const geometry = new THREE.BufferGeometry();
      geometry.setAttribute("position", new THREE.Float32BufferAttribute(positions, 3));
      group.add(new THREE.LineSegments(geometry, new THREE.LineBasicMaterial({
        color: night ? 0xcbb89a : 0x8a7a5e, transparent: true, opacity: 0.16,
      })));
    }
    // suns
    for (const s of g.suns) {
      const scale = 10 + Math.min(Math.sqrt(s.n) * 0.28, 14);
      const sprite = new THREE.Sprite(new THREE.SpriteMaterial({
        map: glow, color: s.color, transparent: true,
        opacity: 0.95, depthWrite: false, blending: THREE.AdditiveBlending,
      }));
      sprite.scale.set(scale, scale, 1);
      sprite.position.copy(s.pos);
      sprite.userData = { kind: "sun", source: s.name, n: s.n };
      group.add(sprite);
      pickables.push(sprite);
      const label = labelSprite(s.name, `#${new THREE.Color(s.color).getHexString()}`, 1.5);
      label.position.copy(s.pos).add(new THREE.Vector3(0, -8, 0));
      group.add(label);
    }
    // planets
    for (const p of g.planets) {
      const scale = 2.2 + Math.min(Math.sqrt(p.total) * 0.28, 5) + p.importance * 0.8;
      const color = TYPE_COLORS[p.entity_type] || 0xb8b2a6;
      const sprite = new THREE.Sprite(new THREE.SpriteMaterial({
        map: glow, color, transparent: true,
        opacity: p.spokes.length > 1 ? 1.0 : 0.8,
        depthWrite: false, blending: THREE.AdditiveBlending,
      }));
      sprite.scale.set(scale, scale, 1);
      sprite.position.copy(p.pos);
      sprite.userData = { kind: "planet", planet: p };
      group.add(sprite);
      pickables.push(sprite);
      // label the biggest + every multi-source bridge
      if (p.total >= (g.planets[Math.min(11, g.planets.length - 1)]?.total || 0)
          || p.spokes.length >= 3 || p.importance > 0) {
        const label = labelSprite(p.name, night ? "#e8dcc8" : "#3a3226", 0.9);
        label.position.copy(p.pos).add(new THREE.Vector3(0, 2.6, 0));
        group.add(label);
      }
    }
    // the Notes sun + its cluster (the desk's thinking, airborne)
    {
      const noteColor = 0xffd9a0;
      const sun = new THREE.Sprite(new THREE.SpriteMaterial({
        map: glow, color: noteColor, transparent: true,
        opacity: 0.9, depthWrite: false, blending: THREE.AdditiveBlending,
      }));
      sun.scale.set(9 + Math.min(g.notes.length * 0.3, 8), 9 + Math.min(g.notes.length * 0.3, 8), 1);
      sun.position.copy(g.notesSun.pos);
      sun.userData = { kind: "notes-sun", n: g.notes.length };
      group.add(sun);
      pickables.push(sun);
      const label = labelSprite("notes", `#${new THREE.Color(noteColor).getHexString()}`, 1.4);
      label.position.copy(g.notesSun.pos).add(new THREE.Vector3(0, -7, 0));
      group.add(label);
      const mentionLines = [];
      for (const n of g.notes) {
        const scale = 1.5 + Math.min(Math.sqrt(n.views || 0) * 0.35, 2.5);
        const sprite = new THREE.Sprite(new THREE.SpriteMaterial({
          map: glow, color: noteColor, transparent: true, opacity: 0.85,
          depthWrite: false, blending: THREE.AdditiveBlending,
        }));
        sprite.scale.set(scale, scale, 1);
        sprite.position.copy(n.pos);
        sprite.userData = { kind: "note", note: n };
        group.add(sprite);
        pickables.push(sprite);
        // connective lines to the people/orgs the note mentions
        for (const id of n.mentions) {
          const p = g.byId.get(id);
          if (p) mentionLines.push(n.pos.x, n.pos.y, n.pos.z, p.pos.x, p.pos.y, p.pos.z);
        }
      }
      if (mentionLines.length) {
        const geometry = new THREE.BufferGeometry();
        geometry.setAttribute("position", new THREE.Float32BufferAttribute(mentionLines, 3));
        group.add(new THREE.LineSegments(geometry, new THREE.LineBasicMaterial({
          color: noteColor, transparent: true, opacity: 0.18,
        })));
      }
    }
    // comets: semantic pairs riding above the plane
    for (const c of g.comets) {
      const material = new THREE.SpriteMaterial({
        map: glow, color: 0x9ad0ff, transparent: true, opacity: 0.8,
        depthWrite: false, blending: THREE.AdditiveBlending,
      });
      for (const [pos, end] of [[c.aPos, c.a], [c.bPos, c.b]]) {
        const sprite = new THREE.Sprite(material.clone());
        sprite.scale.set(1.6, 1.6, 1);
        sprite.position.copy(pos);
        sprite.userData = { kind: "comet", end, pair: c };
        group.add(sprite);
        pickables.push(sprite);
      }
      const geometry = new THREE.BufferGeometry().setFromPoints([c.aPos, c.bPos]);
      group.add(new THREE.Line(geometry, new THREE.LineDashedMaterial({
        color: 0x9ad0ff, transparent: true, opacity: 0.35, dashSize: 1.2, gapSize: 1.0,
      })));
      group.children[group.children.length - 1].computeLineDistances();
    }
    scene.add(group);
    if (opts.fromSunPos) {
      // the ride home: reappear where we were (relative to the sun we
      // were visiting), still looking at it, then glide out wide
      camera.position.add(opts.fromSunPos);
      controls.target.copy(opts.fromSunPos);
      glideTarget = new THREE.Vector3(0, 0, 0);
    } else {
      controls.target.set(0, 0, 0);
    }
    entry.active = true;
    entry.to.set(0, 92, 120);
    if (hud) {
      hud.innerHTML = `
        <div class="hud-title">the whole record</div>
        <div class="meta">${g.suns.length} sources · ${g.planets.length} gravitational bodies</div>
        <div class="meta" style="margin-top:4px">suns are sources; planets are who they cite most.
          A planet hanging <b>between</b> suns bridges your worlds.
          Faint blue comets: episodes that MEAN the same thing across sources.</div>
        <div class="meta" style="margin-top:4px">click a <b>sun</b> to zoom into its system ·
          click a <b>person</b> for their card · anything else → Memories</div>`;
    }
  }

  /* the descent: fly the camera to the sun IN GALAXY SPACE, and only
     when we arrive swap to the system scene with the same relative
     view — motion, not a cut. "notes" descends like any other sun. */
  function descendTo(sunName) {
    const sun = sunName === "notes"
      ? lastLayout?.notesSun
      : lastLayout?.suns.find((s) => s.name === sunName);
    if (!sun) return sunName === "notes" ? buildNotesSystem() : buildSystem(sunName);
    lastSunPos = sun.pos.clone();
    const approach = camera.position.clone().sub(sun.pos).normalize()
      .multiplyScalar(52).add(new THREE.Vector3(0, 22, 0));
    glideTarget = sun.pos.clone();
    entry.active = true;
    entry.to.copy(sun.pos.clone().add(approach));
    entry.onArrive = () => (sunName === "notes"
      ? buildNotesSystem({ fromPos: sun.pos.clone() })
      : buildSystem(sunName, { fromPos: sun.pos.clone() }));
    if (hud) hud.innerHTML = `<div class="meta">approaching ${esc(sunName)}…</div>`;
  }

  /* — the notes system: every note labeled, arranged so shared tags
     sit together, association threads between tag-siblings — */
  async function buildNotesSystem(opts = {}) {
    mode = "system";
    let data;
    try {
      data = await fetchGalaxy(9);
    } catch (err) {
      if (hud) hud.innerHTML = `<div class="meta">${esc(err.message)}</div>`;
      return;
    }
    const noteColor = 0xffd9a0;
    const notes = (data.notes || []).slice();
    // tag-adjacency ordering: primary tag groups sit together on the
    // spiral, so clusters read as arcs
    notes.sort((a, b) =>
      (a.tags[0] || "~").localeCompare(b.tags[0] || "~") || (b.views || 0) - (a.views || 0));

    if (group) scene.remove(group);
    group = new THREE.Group();
    pickables = [];
    const sun = new THREE.Sprite(new THREE.SpriteMaterial({
      map: glow, color: noteColor, transparent: true,
      opacity: 0.95, depthWrite: false, blending: THREE.AdditiveBlending,
    }));
    sun.scale.set(20, 20, 1);
    group.add(sun);
    const sunLabel = labelSprite("notes", `#${new THREE.Color(noteColor).getHexString()}`, 1.6);
    sunLabel.position.set(0, -11, 0);
    group.add(sunLabel);
    const golden = Math.PI * (3 - Math.sqrt(5));
    const positions = [];
    notes.forEach((n, i) => {
      const r = 18 + i * 2.4;
      const a = i * golden;
      const pos = new THREE.Vector3(Math.cos(a) * r, Math.sin(i * 1.7) * 2, Math.sin(a) * r);
      n.pos = pos;
      const scale = 2 + Math.min(Math.sqrt(n.views || 0) * 0.5, 3.5);
      const sprite = new THREE.Sprite(new THREE.SpriteMaterial({
        map: glow, color: noteColor, transparent: true, opacity: 0.9,
        depthWrite: false, blending: THREE.AdditiveBlending,
      }));
      sprite.scale.set(scale, scale, 1);
      sprite.position.copy(pos);
      sprite.userData = { kind: "note", note: n };
      group.add(sprite);
      pickables.push(sprite);
      const label = labelSprite(n.title, night ? "#e8dcc8" : "#3a3226", 0.85);
      label.position.copy(pos).add(new THREE.Vector3(0, 2.2, 0));
      group.add(label);
      // orbit thread
      positions.push(0, 0, 0, pos.x, pos.y, pos.z);
    });
    // association threads: notes sharing a tag connect
    for (let i = 0; i < notes.length; i++) {
      for (let j = i + 1; j < notes.length; j++) {
        if (notes[i].tags.some((t) => notes[j].tags.includes(t))) {
          positions.push(
            notes[i].pos.x, notes[i].pos.y, notes[i].pos.z,
            notes[j].pos.x, notes[j].pos.y, notes[j].pos.z,
          );
        }
      }
    }
    if (positions.length) {
      const geometry = new THREE.BufferGeometry();
      geometry.setAttribute("position", new THREE.Float32BufferAttribute(positions, 3));
      group.add(new THREE.LineSegments(geometry, new THREE.LineBasicMaterial({
        color: noteColor, transparent: true, opacity: 0.12,
      })));
    }
    scene.add(group);
    if (opts.fromPos) {
      camera.position.sub(opts.fromPos);
    }
    controls.target.set(0, 0, 0);
    entry.active = true;
    entry.to.set(0, 55, 78);
    if (hud) {
      hud.innerHTML = `
        <div class="hud-title">notes</div>
        <div class="meta">${notes.length} live notes — arranged so shared tags sit
          together; threads join tag-siblings</div>
        <div class="meta" style="margin-top:4px">click a note to open it ·
          background or Esc → back to the galaxy</div>
        <div class="meta" style="margin-top:4px">
          <button class="btn btn-quiet" id="gx-notes-panel">open the notes panel →</button>
          <button class="btn btn-quiet" id="gx-back">◌ whole galaxy</button></div>`;
      hud.querySelector("#gx-notes-panel").onclick = () => onNotesSun?.();
      hud.querySelector("#gx-back").onclick = () =>
        build(...currentWindow, { fromSunPos: lastSunPos });
    }
  }

  /* — one sun's system: its bodies on clean ranked orbits — */
  async function buildSystem(sunName, opts = {}) {
    mode = "system";
    if (hud) hud.innerHTML = `<div class="meta">entering ${esc(sunName)}…</div>`;
    let data;
    try {
      data = await fetchGalaxy(24); // deeper roster for the close-up
    } catch (err) {
      if (hud) hud.innerHTML = `<div class="meta">${esc(err.message)}</div>`;
      return;
    }
    const sunMeta = data.sources.find(([s]) => s === sunName);
    const sunColor = SOURCE_COLORS[
      Math.max(data.sources.findIndex(([s]) => s === sunName), 0) % SOURCE_COLORS.length];
    const bodies = data.nodes
      .map((n) => ({ ...n, here: (n.spokes.find(([s]) => s === sunName) || [null, 0])[1] }))
      .filter((n) => n.here > 0)
      .sort((a, b) => b.here - a.here)
      .slice(0, 28);

    if (group) scene.remove(group);
    group = new THREE.Group();
    pickables = [];
    // the sun, centered
    const sun = new THREE.Sprite(new THREE.SpriteMaterial({
      map: glow, color: sunColor, transparent: true,
      opacity: 0.95, depthWrite: false, blending: THREE.AdditiveBlending,
    }));
    sun.scale.set(22, 22, 1);
    sun.userData = { kind: "sun-here", source: sunName };
    group.add(sun);
    const sunLabel = labelSprite(sunName, `#${new THREE.Color(sunColor).getHexString()}`, 1.6);
    sunLabel.position.set(0, -12, 0);
    group.add(sunLabel);
    // ranked orbital rings, golden-angle spread, everyone labeled
    const golden = Math.PI * (3 - Math.sqrt(5));
    bodies.forEach((b, i) => {
      const r = 20 + i * 2.6;
      const a = i * golden;
      const pos = new THREE.Vector3(Math.cos(a) * r, Math.sin(i * 1.7) * 2.5, Math.sin(a) * r);
      const scale = 2.4 + Math.min(Math.sqrt(b.here) * 0.5, 6) + b.importance * 0.8;
      const color = TYPE_COLORS[b.entity_type] || 0xb8b2a6;
      const sprite = new THREE.Sprite(new THREE.SpriteMaterial({
        map: glow, color, transparent: true, opacity: 0.95,
        depthWrite: false, blending: THREE.AdditiveBlending,
      }));
      sprite.scale.set(scale, scale, 1);
      sprite.position.copy(pos);
      sprite.userData = { kind: "planet", planet: b, system: sunName };
      group.add(sprite);
      pickables.push(sprite);
      const label = labelSprite(b.name, night ? "#e8dcc8" : "#3a3226", 0.85);
      label.position.copy(pos).add(new THREE.Vector3(0, 2.4, 0));
      group.add(label);
      // orbit thread back to the sun
      const geometry = new THREE.BufferGeometry().setFromPoints([new THREE.Vector3(), pos]);
      group.add(new THREE.Line(geometry, new THREE.LineBasicMaterial({
        color: sunColor, transparent: true, opacity: 0.10,
      })));
    });
    scene.add(group);
    if (opts.fromPos) {
      // seamless: we flew TO the sun in galaxy space; re-express the
      // same relative view around the system's origin
      camera.position.sub(opts.fromPos);
      controls.target.set(0, 0, 0);
    }
    entry.active = true;
    entry.to.set(0, 55, 78);
    if (!opts.fromPos) controls.target.set(0, 0, 0);
    if (hud) {
      hud.innerHTML = `
        <div class="hud-title">${esc(sunName)}</div>
        <div class="meta">${(sunMeta?.[1] ?? 0).toLocaleString()} episodes ·
          ${bodies.length} orbiting bodies (evidence-ranked)</div>
        <div class="meta" style="margin-top:4px">click a <b>person</b> → their card ·
          anything else → Memories · background or Esc → back to the galaxy</div>
        <div class="meta" style="margin-top:4px">
          <button class="btn btn-quiet" id="gx-browse">browse these episodes →</button>
          <button class="btn btn-quiet" id="gx-back">◌ whole galaxy</button></div>`;
      hud.querySelector("#gx-browse").onclick = () => onBrowseSource?.(sunName);
      hud.querySelector("#gx-back").onclick = () =>
        build(...currentWindow, { fromSunPos: lastSunPos });
    }
  }

  await build();

  /* — hover + click — */
  const raycaster = new THREE.Raycaster();
  const pointer = new THREE.Vector2();
  let hovered = null;
  const pick = (event) => {
    const rect = canvas.getBoundingClientRect();
    pointer.x = ((event.clientX - rect.left) / rect.width) * 2 - 1;
    pointer.y = -((event.clientY - rect.top) / rect.height) * 2 + 1;
    raycaster.setFromCamera(pointer, camera);
    const hit = raycaster.intersectObjects(pickables, false)[0];
    return hit?.object || null;
  };
  canvas.addEventListener("pointermove", (event) => {
    const object = pick(event);
    if (object === hovered) return;
    hovered = object;
    canvas.style.cursor = object ? "pointer" : "";
    if (!object || !hud) return;
    const u = object.userData;
    if (u.kind === "sun") {
      hud.innerHTML = `<div class="hud-title">${esc(u.source)}</div>
        <div class="meta">${u.n.toLocaleString()} episodes in this window · click to zoom into its system</div>`;
    } else if (u.kind === "planet") {
      hud.innerHTML = `<div class="hud-title">${esc(u.planet.name)}</div>
        <div class="meta">${esc(u.planet.entity_type)}${u.planet.importance ? " " + "★".repeat(u.planet.importance) : ""}
          · ${u.planet.total} citations</div>
        <div class="meta">${fmtSpokes(u.planet)}</div>
        <div class="meta">${u.planet.entity_type === "Person"
          ? "click → their card" : "click → Memories about them"}</div>`;
    } else if (u.kind === "note") {
      hud.innerHTML = `<div class="hud-title">${esc(u.note.title)}</div>
        <div class="meta">a live note${u.note.views > 1 ? ` · read ×${u.note.views}` : ""}${
          u.note.tags.length ? ` · ${u.note.tags.map((t) => `#${esc(t)}`).join(" ")}` : ""}</div>
        <div class="meta">${u.note.mentions.length
          ? `linked to ${u.note.mentions.length} ${u.note.mentions.length === 1 ? "person/org" : "people/orgs"} it @-mentions`
          : "mentions no one yet"} · click to open</div>`;
    } else if (u.kind === "notes-sun") {
      hud.innerHTML = `<div class="hud-title">notes</div>
        <div class="meta">${u.n} live notes — the desk's thinking, clustered by tags,
          tethered to the people they @-mention · click to zoom in</div>`;
    } else if (u.kind === "comet") {
      hud.innerHTML = `<div class="hud-title">semantic pair · ${(u.pair.score * 100).toFixed(0)}%</div>
        <div class="meta">${esc(u.pair.a.title)} <span style="opacity:.6">(${esc(u.pair.a.source)}, ${esc(u.pair.a.day)})</span></div>
        <div class="meta">${esc(u.pair.b.title)} <span style="opacity:.6">(${esc(u.pair.b.source)}, ${esc(u.pair.b.day)})</span></div>
        <div class="meta">two moments that rhyme across sources · click → that day</div>`;
    }
  });
  let downAt = null;
  canvas.addEventListener("pointerdown", (e) => (downAt = [e.clientX, e.clientY]));
  canvas.addEventListener("click", (event) => {
    // a drag that ends over the canvas is not a click
    if (downAt && Math.hypot(event.clientX - downAt[0], event.clientY - downAt[1]) > 4) return;
    const object = pick(event);
    if (!object) {
      if (mode === "system") build(...currentWindow, { fromSunPos: lastSunPos }); // ascend
      return;
    }
    const u = object.userData;
    if (u.kind === "sun") descendTo(u.source);
    else if (u.kind === "notes-sun") descendTo("notes");
    else if (u.kind === "planet") {
      // people and projects open their card (stay in context); other
      // entities go to Memories
      if (u.planet.entity_type === "Person" || u.planet.entity_type === "Project")
        onPerson?.(u.planet.name);
      else onMemories?.(u.planet.name, u.system || null);
    } else if (u.kind === "comet") onDay?.(u.end.day);
    else if (u.kind === "note") {
      // Zoomed all the way out, note nodes are tiny and easy to mis-hit —
      // a click there descends into the Notes cluster FIRST; only once
      // inside the notes system does a click open the individual note (item 7).
      if (mode === "system") onNote?.(u.note.id);
      else descendTo("notes");
    }
  });
  const onEsc = (e) => {
    if (e.key === "Escape" && mode === "system") build(...currentWindow, { fromSunPos: lastSunPos });
  };
  window.addEventListener("keydown", onEsc);

  const animate = () => {
    frame = requestAnimationFrame(animate);
    if (glideTarget) {
      controls.target.lerp(glideTarget, 0.06);
      if (controls.target.distanceTo(glideTarget) < 0.5) glideTarget = null;
    }
    if (entry.active) {
      camera.position.lerp(entry.to, 0.055);
      if (camera.position.distanceTo(entry.to) < 1.5) {
        entry.active = false;
        const arrive = entry.onArrive;
        entry.onArrive = null;
        arrive?.();
      }
    }
    controls.update();
    renderer.render(scene, camera);
  };
  animate();

  const onResize = () => {
    const w = parent.clientWidth, h = parent.clientHeight;
    camera.aspect = w / h;
    camera.updateProjectionMatrix();
    renderer.setSize(w, h, false);
  };
  window.addEventListener("resize", onResize);

  return {
    setWindow: (from, to) => build(from, to),
    // deep links + verification: the same cinematic descent as a click
    enterSystem: (sun) => (lastLayout ? descendTo(sun) : buildSystem(sun)),
    ascend: () => build(...currentWindow, { fromSunPos: lastSunPos }),
    mode: () => mode,
    destroy: () => {
      cancelAnimationFrame(frame);
      window.removeEventListener("resize", onResize);
      window.removeEventListener("keydown", onEsc);
      renderer.dispose();
    },
  };
}
