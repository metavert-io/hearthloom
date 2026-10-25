// The constellation: an entity's 1–2 hop neighborhood rendered as warm
// light in darkness. Deliberately *local* — the design doc forbids the
// global hairball; this is legible specificity with atmosphere.

import * as THREE from "three";
import { OrbitControls } from "three/addons/controls/OrbitControls.js";

const TYPE_COLORS = {
  Person: 0xffb26b,
  Project: 0xffd166,
  Place: 0x7fd7c4,
  Org: 0xef8354,
  Event: 0xc9a7eb,
  Artifact: 0x8b93a7,
};
const TYPE_FALLBACK = 0xb8b2a6;
// Legibility caps: a constellation, never a hairball.
const MAX_RING1 = 34;
const MAX_PER_PARENT = 5;
const MAX_NODES = 150;

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
  const shown = text.length > 34 ? text.slice(0, 33) + "…" : text;
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

/** Assemble a legible subgraph from raw traverse hops. */
function buildGraph(hops) {
  const nodes = new Map();
  const links = [];
  const seen = new Set();
  let truncated = 0;
  const add = (row, depth) => {
    let node = nodes.get(row.id);
    if (!node) {
      node = { id: row.id, name: row.name, type: row.entity_type, depth, children: 0 };
      nodes.set(row.id, node);
    } else if (depth < node.depth) node.depth = depth;
    return node;
  };
  const link = (a, b, relation) => {
    const key = [a, b].sort().join("|");
    if (!seen.has(key)) {
      seen.add(key);
      links.push({ a, b, relation });
    }
  };
  let center = null;
  let ring1Count = 0;
  let artifactCount = 0;
  // People, projects, places, orgs first; file Artifacts are supporting
  // cast (capped hard) so a repo's constellation isn't a file listing.
  const ring1Hops = hops
    .filter((h) => h.depth === 1)
    .sort((a, b) =>
      (a.to.entity_type === "Artifact" ? 1 : 0) - (b.to.entity_type === "Artifact" ? 1 : 0));
  const MAX_ARTIFACTS = 8;
  for (const hop of ring1Hops) {
    if (!center) center = add(hop.from, 0);
    const isArtifact = hop.to.entity_type === "Artifact";
    if (ring1Count >= MAX_RING1 || (isArtifact && artifactCount >= MAX_ARTIFACTS)) {
      truncated++;
      continue;
    }
    if (!nodes.has(hop.to.id)) {
      ring1Count++;
      if (isArtifact) artifactCount++;
    }
    const ring1Node = add(hop.to, 1);
    if (!ring1Node.relation) { ring1Node.relation = hop.relation; ring1Node.via = hop.from.name; }
    link(hop.from.id, hop.to.id, hop.relation);
  }
  for (const hop of hops.filter((h) => h.depth === 2)) {
    const parent = nodes.get(hop.from.id);
    if (!parent || parent.depth !== 1) continue;
    if (nodes.size >= MAX_NODES || parent.children >= MAX_PER_PARENT) { truncated++; continue; }
    if (!nodes.has(hop.to.id)) parent.children++;
    const ring2Node = add(hop.to, 2);
    if (!ring2Node.relation) { ring2Node.relation = hop.relation; ring2Node.via = hop.from.name; }
    link(hop.from.id, hop.to.id, hop.relation);
  }
  return { center, nodes, links, truncated };
}

/** Radial seed + light repulsion: stable, fast, no physics engine. */
function layout(graph) {
  const golden = Math.PI * (3 - Math.sqrt(5));
  if (graph.center) graph.center.pos = new THREE.Vector3(0, 0, 0);
  const ring1 = [...graph.nodes.values()].filter((n) => n.depth === 1);
  const ring2 = [...graph.nodes.values()].filter((n) => n.depth === 2);
  ring1.forEach((node, i) => {
    const angle = i * golden;
    const y = ((i % 7) / 6 - 0.5) * 16;
    const r = 26 + (i % 3) * 3;
    node.pos = new THREE.Vector3(Math.cos(angle) * r, y, Math.sin(angle) * r);
  });
  const parentOf = {};
  for (const l of graph.links) {
    const a = graph.nodes.get(l.a), b = graph.nodes.get(l.b);
    if (a?.depth === 1 && b?.depth === 2) parentOf[b.id] = a;
    if (b?.depth === 1 && a?.depth === 2) parentOf[a.id] = b;
  }
  ring2.forEach((node, i) => {
    const parent = parentOf[node.id];
    const base = parent?.pos?.clone() || new THREE.Vector3(34, 0, 0);
    const out = base.clone().setY(0).normalize().multiplyScalar(12 + (i % 3) * 2.5);
    node.pos = base.add(out).add(new THREE.Vector3(
      Math.sin(i * 12.9898) * 5,
      Math.cos(i * 78.233) * 5,
      Math.sin(i * 3.7 + 1) * 5,
    ));
  });
  const all = [...graph.nodes.values()].filter((n) => n.depth > 0);
  for (let iter = 0; iter < 50; iter++) {
    for (let i = 0; i < all.length; i++) {
      for (let j = i + 1; j < all.length; j++) {
        const a = all[i], b = all[j];
        const delta = a.pos.clone().sub(b.pos);
        const dist = Math.max(delta.length(), 0.1);
        if (dist < 7.5) {
          const push = delta.normalize().multiplyScalar((7.5 - dist) * 0.16);
          a.pos.add(push);
          b.pos.sub(push);
        }
      }
    }
  }
}

export async function mount(canvas, focusName, { onFocus, onOpen, hud, api, esc }) {
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
  scene.fog = new THREE.FogExp2(bg, 0.0075);

  const camera = new THREE.PerspectiveCamera(48, width / height, 0.1, 800);
  // cinematic entry: begin far out and glide to the home framing
  camera.position.set(0, 46, 190);
  const entry = { active: true, to: new THREE.Vector3(0, 16, 70) };
  const controls = new OrbitControls(camera, canvas);
  controls.enableDamping = true;
  controls.dampingFactor = 0.06;
  controls.minDistance = 20;
  controls.maxDistance = 240;
  controls.enablePan = true;
  controls.screenSpacePanning = true;
  controls.autoRotate = true;
  controls.autoRotateSpeed = 0.35;
  canvas.addEventListener("pointerdown", () => (controls.autoRotate = false), { once: true });

  // Explicit navigation: on-screen pad + arrow keys — orbiting into a
  // corner must never strand you.
  const home = { position: entry.to.clone(), target: new THREE.Vector3() };
  // smooth programmatic orbit around the current center: remaining
  // radians are consumed a little each frame
  const spin = { azimuth: 0, polar: 0 };
  const pan = (dx, dy) => {
    controls.autoRotate = false;
    const offset = new THREE.Vector3();
    const right = new THREE.Vector3().setFromMatrixColumn(camera.matrix, 0);
    const up = new THREE.Vector3().setFromMatrixColumn(camera.matrix, 1);
    const scale = camera.position.distanceTo(controls.target) * 0.12;
    offset.addScaledVector(right, dx * scale).addScaledVector(up, dy * scale);
    camera.position.add(offset);
    controls.target.add(offset);
  };
  const zoom = (factor) => {
    controls.autoRotate = false;
    const dir = camera.position.clone().sub(controls.target);
    const len = THREE.MathUtils.clamp(dir.length() * factor, controls.minDistance, controls.maxDistance);
    camera.position.copy(controls.target).addScaledVector(dir.normalize(), len);
  };
  const act = (which) => {
    if (which === "up") pan(0, 1);
    else if (which === "down") pan(0, -1);
    else if (which === "left") pan(-1, 0);
    else if (which === "right") pan(1, 0);
    else if (which === "in") zoom(0.8);
    else if (which === "out") zoom(1.25);
    else if (which === "rotl") { controls.autoRotate = false; spin.azimuth += Math.PI / 5; }
    else if (which === "rotr") { controls.autoRotate = false; spin.azimuth -= Math.PI / 5; }
    else if (which === "tiltu") { controls.autoRotate = false; spin.polar -= Math.PI / 10; }
    else if (which === "tiltd") { controls.autoRotate = false; spin.polar += Math.PI / 10; }
    else if (which === "tour") {
      controls.autoRotate = !controls.autoRotate;
      controls.autoRotateSpeed = 0.6;
      const button = document.getElementById("tour-btn");
      if (button) button.textContent = controls.autoRotate ? "⏸" : "▶";
    } else if (which === "home") {
      entry.active = true;
      entry.to.copy(home.position);
      controls.target.copy(home.target);
      spin.azimuth = 0;
      spin.polar = 0;
    }
  };
  document.getElementById("navpad")?.addEventListener("click", (event) => {
    const which = event.target.closest("button")?.dataset.pan;
    if (which) act(which);
  });
  const onKey = (event) => {
    if (event.target.tagName === "INPUT" || event.target.tagName === "TEXTAREA") return;
    const map = { ArrowUp: "up", ArrowDown: "down", ArrowLeft: "left", ArrowRight: "right",
      "+": "in", "=": "in", "-": "out", "0": "home", "[": "rotl", "]": "rotr", "t": "tour" };
    if (map[event.key]) { act(map[event.key]); event.preventDefault(); }
  };
  window.addEventListener("keydown", onKey);

  // ambient starfield — the memory beyond this neighborhood
  {
    const count = 1100;
    const positions = new Float32Array(count * 3);
    for (let i = 0; i < count; i++) {
      const r = 150 + Math.random() * 260;
      const theta = Math.random() * Math.PI * 2;
      const phi = Math.acos(2 * Math.random() - 1);
      positions[i * 3] = r * Math.sin(phi) * Math.cos(theta);
      positions[i * 3 + 1] = r * Math.cos(phi) * 0.55;
      positions[i * 3 + 2] = r * Math.sin(phi) * Math.sin(theta);
    }
    const geometry = new THREE.BufferGeometry();
    geometry.setAttribute("position", new THREE.BufferAttribute(positions, 3));
    scene.add(new THREE.Points(geometry, new THREE.PointsMaterial({
      size: 0.8,
      color: night ? 0x6f6455 : 0xa89a86,
      transparent: true,
      opacity: 0.55,
      sizeAttenuation: true,
    })));
  }

  // fetch + build
  if (hud) hud.innerHTML = `<div class="meta">tracing ${esc(focusName)}…</div>`;
  let graph;
  try {
    // Significance-first progressive loading: the server sends each
    // node's strongest edges only (top=), so the first paint is the
    // constellation's brightest stars in milliseconds — clicking any
    // star refocuses (an instant re-fetch), which IS the zoom-outward
    // expansion. Uncapped depth-2 took 140s on the post-mail graph.
    const data = await api(`/v1/traverse?start=${encodeURIComponent(focusName)}&depth=2&top=30`);
    graph = buildGraph(data.hops || []);
    if (!graph.center) {
      // an entity with no edges still deserves a star
      graph.center = { id: "solo", name: focusName, type: "Entity", depth: 0, pos: new THREE.Vector3() };
      graph.nodes.set("solo", graph.center);
    }
  } catch (err) {
    if (hud) hud.innerHTML = `<div class="meta">${esc(err.message)}</div>`;
    return;
  }
  layout(graph);

  const glow = glowTexture();
  const clickable = [];
  const nodeGroup = new THREE.Group();
  scene.add(nodeGroup);
  let labeledArtifacts = 0;

  for (const node of graph.nodes.values()) {
    const color = TYPE_COLORS[node.type] ?? TYPE_FALLBACK;
    const size = node.depth === 0 ? 11 : node.depth === 1 ? 5 : 2.6;
    const sprite = new THREE.Sprite(new THREE.SpriteMaterial({
      map: glow,
      color,
      transparent: true,
      opacity: node.depth === 2 ? 0.55 : 0.8,
      depthWrite: false,
      blending: THREE.AdditiveBlending,
    }));
    sprite.position.copy(node.pos);
    sprite.scale.setScalar(size);
    sprite.userData = node;
    nodeGroup.add(sprite);
    clickable.push(sprite);
    node.sprite = sprite;

    // saturated core so the type color reads, not just white haze
    const core = new THREE.Sprite(new THREE.SpriteMaterial({
      map: glow, color, transparent: true, opacity: 1, depthWrite: false,
    }));
    core.position.copy(node.pos);
    core.scale.setScalar(size * 0.34);
    nodeGroup.add(core);
    node.core = core;

    // Labels: center always; ring-1 non-Artifacts; Artifacts get short
    // basenames and only a few — legibility beats completeness.
    const isArtifact = node.type === "Artifact";
    const shown = isArtifact ? node.name.split("/").pop() : node.name;
    labeledArtifacts += isArtifact && node.depth === 1 ? 1 : 0;
    if (node.depth === 0 || (node.depth === 1 && (!isArtifact || labeledArtifacts <= 5))) {
      const css = "#" + color.toString(16).padStart(6, "0");
      const label = labelSprite(shown, node.depth === 0 ? (night ? "#f0e7d8" : "#2c241a") : css,
        node.depth === 0 ? 1.25 : isArtifact ? 0.62 : 0.85);
      label.material.opacity = isArtifact ? 0.7 : 1;
      node.label = label;
      label.position.copy(node.pos).add(new THREE.Vector3(0, node.depth === 0 ? 9 : 5.2, 0));
      nodeGroup.add(label);
    }
  }

  // edges: warm additive threads
  {
    const positions = [];
    for (const l of graph.links) {
      const a = graph.nodes.get(l.a), b = graph.nodes.get(l.b);
      if (!a?.pos || !b?.pos) continue;
      positions.push(a.pos.x, a.pos.y, a.pos.z, b.pos.x, b.pos.y, b.pos.z);
    }
    const geometry = new THREE.BufferGeometry();
    geometry.setAttribute("position", new THREE.BufferAttribute(new Float32Array(positions), 3));
    scene.add(new THREE.LineSegments(geometry, new THREE.LineBasicMaterial({
      color: night ? 0xe8833a : 0xb4531d,
      transparent: true,
      opacity: night ? 0.28 : 0.35,
      blending: night ? THREE.AdditiveBlending : THREE.NormalBlending,
    })));
  }

  // HUD
  if (hud) {
    const counts = {};
    for (const n of graph.nodes.values()) counts[n.type] = (counts[n.type] || 0) + 1;
    hud.innerHTML = `
      <h2>${esc(graph.center.name)}</h2>
      <div class="meta">${graph.nodes.size} bodies · ${graph.links.length} threads${
        graph.truncated ? ` · ${graph.truncated} beyond the veil` : ""} ·
        <a href="#/entity/${encodeURIComponent(graph.center.name)}">open dossier</a></div>
      <div class="legend">${Object.entries(counts)
        .map(([type, n]) => {
          const c = "#" + (TYPE_COLORS[type] ?? TYPE_FALLBACK).toString(16).padStart(6, "0");
          return `<span><i style="background:${c};box-shadow:0 0 6px ${c}"></i>${esc(type)} ${n}</span>`;
        }).join("")}</div>`;
  }

  // interaction: click refocus, double-click dossier
  const raycaster = new THREE.Raycaster();
  const pointer = new THREE.Vector2();
  let lastClick = 0;
  canvas.addEventListener("click", (event) => {
    const rect = canvas.getBoundingClientRect();
    pointer.x = ((event.clientX - rect.left) / rect.width) * 2 - 1;
    pointer.y = -((event.clientY - rect.top) / rect.height) * 2 + 1;
    raycaster.setFromCamera(pointer, camera);
    const hit = raycaster.intersectObjects(clickable)[0];
    if (!hit) return;
    const node = hit.object.userData;
    const now = performance.now();
    if (now - lastClick < 350) onOpen(node.name);
    else if (node.depth !== 0) onFocus(node.name);
    lastClick = now;
  });

  // gentle life: ember pulse + drift
  const t0 = performance.now();
  const spherical = new THREE.Spherical();
  const animate = () => {
    frame = requestAnimationFrame(animate);
    const t = (performance.now() - t0) / 1000;

    // consume programmatic orbit a little per frame (eased)
    if (Math.abs(spin.azimuth) > 0.001 || Math.abs(spin.polar) > 0.001) {
      const stepA = spin.azimuth * 0.12;
      const stepP = spin.polar * 0.12;
      spin.azimuth -= stepA;
      spin.polar -= stepP;
      const offset = camera.position.clone().sub(controls.target);
      spherical.setFromVector3(offset);
      spherical.theta += stepA;
      spherical.phi = THREE.MathUtils.clamp(spherical.phi + stepP, 0.15, Math.PI - 0.15);
      offset.setFromSpherical(spherical);
      camera.position.copy(controls.target).add(offset);
    }
    // cinematic glide (entry and ⌂)
    if (entry.active) {
      camera.position.lerp(entry.to, 0.06);
      if (camera.position.distanceTo(entry.to) < 0.5) entry.active = false;
    }
    for (const node of graph.nodes.values()) {
      if (!node.sprite) continue;
      const base = node.depth === 0 ? 11 : node.depth === 1 ? 5 : 2.6;
      const pulse = node.depth === 0 ? 1 + Math.sin(t * 1.4) * 0.06
        : 1 + Math.sin(t * 0.9 + node.pos.x) * 0.05;
      node.sprite.scale.setScalar(base * pulse * (node.visualScale ?? 1));
    }
    controls.update();
    renderer.render(scene, camera);
  };
  animate();

  // responsive
  const onResize = () => {
    const w = parent.clientWidth, h = parent.clientHeight;
    camera.aspect = w / h;
    camera.updateProjectionMatrix();
    renderer.setSize(w, h, false);
  };
  window.addEventListener("resize", onResize);
  // The wrap's size can settle AFTER mount (fonts, the bottom overlay,
  // .view padding) — a stale measurement renders the scene offset
  // toward the lower right. Re-measure on first paint and on any
  // element resize, not just window resize.
  requestAnimationFrame(onResize);
  const wrapWatcher = new ResizeObserver(onResize);
  wrapWatcher.observe(parent);

  /* — the sky tonight: evidence in the active window sizes and lights
     each star; stars with nothing in the window cool to ash instead of
     vanishing (the past stays visible, dimly). Restyle, not rebuild —
     spinning the era dial keeps the layout stable. — */
  let maxEvidence = 1;
  async function setWindow(from, to) {
    const ids = [...graph.nodes.values()].map((n) => n.id).filter((id) => id !== "solo");
    if (!ids.length) return;
    try {
      const res = await fetch("/v1/constellation/evidence", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ ids, from, to }),
      });
      const { evidence } = await res.json();
      maxEvidence = Math.max(1, ...Object.values(evidence).map((e) => e.n));
      let lit = 0, total = 0;
      for (const node of graph.nodes.values()) {
        const ev = evidence[node.id];
        node.evidence = ev || null;
        if (node.depth === 0) continue; // the focus always burns
        total++;
        if (ev && ev.n > 0) {
          lit++;
          node.visualScale = 0.8 + 0.85 * Math.sqrt(ev.n / maxEvidence);
          node.sprite.material.opacity = node.depth === 2 ? 0.7 : 1;
          if (node.core) node.core.material.opacity = 1;
          if (node.label) node.label.material.opacity = Math.min(1, 0.7 + ev.n / maxEvidence);
        } else {
          node.visualScale = 0.4; // cooled ash
          node.sprite.material.opacity = 0.08;
          if (node.core) node.core.material.opacity = 0.12;
          if (node.label) node.label.material.opacity = 0.15;
        }
      }
      // A felt confirmation that the era changed.
      const card = document.getElementById("hud-card");
      if (card) {
        let era = card.querySelector(".hud-era");
        if (!era) {
          era = document.createElement("div");
          era.className = "hud-era meta";
          card.appendChild(era);
        }
        era.textContent = `${lit} of ${total} stars lit · ${from} → ${to}`;
      }
      window.__skyLastLit = { lit, total, from, to };
    } catch { /* daemon busy — keep current styling */ }
  }

  // why-connected hover: name · relation via parent · window evidence
  const tip = hud ? null : null; // tooltip rides the hud card
  canvas.addEventListener("pointermove", (event) => {
    const rect = canvas.getBoundingClientRect();
    pointer.x = ((event.clientX - rect.left) / rect.width) * 2 - 1;
    pointer.y = -((event.clientY - rect.top) / rect.height) * 2 + 1;
    raycaster.setFromCamera(pointer, camera);
    const hit = raycaster.intersectObjects(clickable)[0];
    canvas.style.cursor = hit ? "pointer" : "";
    const card = document.getElementById("hud-card");
    if (!card) return;
    if (hit) {
      const n = hit.object.userData;
      const ev = n.evidence;
      card.innerHTML = `<b>${esc(n.name)}</b> <span class="meta">${esc(n.type)}</span><br>
        ${n.relation ? `<span class="meta">${esc(n.relation)} · via ${esc(n.via || "")}</span><br>` : ""}
        <span class="meta">${ev && ev.n
          ? `${ev.n.toLocaleString()} episode${ev.n === 1 ? "" : "s"} in this era · last ${esc(ev.last)}`
          : "quiet in this era"}</span>`;
    }
  });

  return { setWindow };
}
