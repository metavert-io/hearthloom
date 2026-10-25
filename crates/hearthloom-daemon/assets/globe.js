// globe.js — the Atlas embers on a slowly turning sphere. Same vendored
// geometry (painted onto a canvas texture), same points; pure spectacle.
import * as THREE from "three";
import { OrbitControls } from "three/addons/controls/OrbitControls.js";

export function mountGlobe(host, { points, world, colors }) {
  const gcanvas = document.createElement("canvas");
  gcanvas.className = "atlas-globe-canvas";
  host.appendChild(gcanvas);
  const rect = host.getBoundingClientRect();
  const renderer = new THREE.WebGLRenderer({ canvas: gcanvas, antialias: true, alpha: true });
  renderer.setPixelRatio(Math.min(devicePixelRatio, 2));
  renderer.setSize(rect.width, rect.height, false);
  const scene = new THREE.Scene();
  const camera = new THREE.PerspectiveCamera(40, rect.width / rect.height, 0.1, 100);
  camera.position.set(0, 0.6, 3);
  const orbit = new OrbitControls(camera, gcanvas);
  orbit.enableDamping = true;
  orbit.autoRotate = true;
  orbit.autoRotateSpeed = 0.7;
  orbit.minDistance = 1.6;
  orbit.maxDistance = 6;

  // land texture from the vendored polys
  const tex = document.createElement("canvas");
  tex.width = 2048; tex.height = 1024;
  const tctx = tex.getContext("2d");
  tctx.fillStyle = colors.land;
  tctx.strokeStyle = colors.coast;
  const px = (lon, lat) => [(lon + 180) / 360 * 2048, (90 - lat) / 180 * 1024];
  for (const poly of world.polys) {
    tctx.beginPath();
    const [x0, y0] = px(poly[0][0], poly[0][1]);
    tctx.moveTo(x0, y0);
    for (let i = 1; i < poly.length; i++) {
      const [x, y] = px(poly[i][0], poly[i][1]);
      tctx.lineTo(x, y);
    }
    tctx.closePath();
    tctx.fill();
    tctx.stroke();
  }
  const sphere = new THREE.Mesh(
    new THREE.SphereGeometry(1, 64, 48),
    new THREE.MeshBasicMaterial({ map: new THREE.CanvasTexture(tex), transparent: true, opacity: 0.96 }),
  );
  scene.add(sphere);

  const xyz = (lat, lon, r) => {
    const phi = (90 - lat) * Math.PI / 180;
    const theta = (lon + 180) * Math.PI / 180;
    return [-r * Math.sin(phi) * Math.cos(theta), r * Math.cos(phi), r * Math.sin(phi) * Math.sin(theta)];
  };
  const positions = new Float32Array(points.length * 3);
  points.forEach((p, i) => positions.set(xyz(p.lat, p.lon, 1.012), i * 3));
  const geo = new THREE.BufferGeometry();
  geo.setAttribute("position", new THREE.BufferAttribute(positions, 3));
  scene.add(new THREE.Points(geo, new THREE.PointsMaterial({
    color: new THREE.Color(colors.ember), size: 0.022,
    transparent: true, opacity: 0.9, blending: THREE.AdditiveBlending, depthWrite: false,
  })));

  let raf = 0;
  const spin = () => { orbit.update(); renderer.render(scene, camera); raf = requestAnimationFrame(spin); };
  spin();
  return {
    dispose() {
      cancelAnimationFrame(raf);
      orbit.dispose();
      renderer.dispose();
      gcanvas.remove();
    },
  };
}
