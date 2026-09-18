import init, { Emulator } from "./pkg/gba.js";

const TEST_ROM =
  "https://raw.githubusercontent.com/jsmolka/gba-tests/master/arm/arm.gba";

const canvas = document.getElementById("screen");
const ctx = canvas.getContext("2d");
const statusEl = document.getElementById("status");
const overlay = document.getElementById("overlay");
const startBtn = document.getElementById("start");
const demoBtn = document.getElementById("demo");
const romInput = document.getElementById("rom");

const image = ctx.createImageData(240, 160);
let emu = null;
let romName = "";
let saveKey = "";
let audioCtx = null;
let sink = null;
let backlog = 0;
let running = false;

function status(text) {
  statusEl.textContent = text;
}

// Keyboard, matching the desktop build and the README table. The values are
// KEYINPUT bit positions: A, B, Select, Start, Right, Left, Up, Down, R, L.
const KEYS = {
  KeyZ: 0,
  KeyX: 1,
  ShiftRight: 2,
  Enter: 3,
  ArrowRight: 4,
  ArrowLeft: 5,
  ArrowUp: 6,
  ArrowDown: 7,
  KeyW: 8,
  KeyQ: 9,
};
let pressed = 0;
let touchPressed = 0;

function key(e, down) {
  if (!(e.code in KEYS)) return;
  const bit = 1 << KEYS[e.code];
  pressed = down ? pressed | bit : pressed & ~bit;
  e.preventDefault();
}

// On-screen controls for coarse-pointer devices. Buttons hold their KEYINPUT
// bit in data-bit; the d-pad is one surface that resolves each active pointer
// to up to two direction bits so diagonals work, and a thumb can slide from
// one direction to another without lifting.
const touchEl = document.getElementById("touch");
const isTouch = window.matchMedia("(pointer: coarse)").matches;

function setupTouch() {
  document.body.classList.add("touch-on");
  touchEl.hidden = false;
  for (const el of document.querySelectorAll(".touch-note")) el.hidden = false;

  for (const btn of touchEl.querySelectorAll(".tbtn")) {
    const bit = 1 << Number(btn.dataset.bit);
    const down = (e) => {
      touchPressed |= bit;
      btn.classList.add("pressed");
      if (e.cancelable) e.preventDefault();
    };
    const up = () => {
      touchPressed &= ~bit;
      btn.classList.remove("pressed");
    };
    btn.addEventListener("pointerdown", down);
    btn.addEventListener("pointerup", up);
    btn.addEventListener("pointercancel", up);
    btn.addEventListener("pointerleave", up);
    btn.addEventListener("contextmenu", (e) => e.preventDefault());
  }

  const dpad = document.getElementById("dpad");
  const DPAD_MASK = 0b11110000; // Right, Left, Up, Down
  const arms = {
    16: dpad.querySelector(".right"),
    32: dpad.querySelector(".left"),
    64: dpad.querySelector(".up"),
    128: dpad.querySelector(".down"),
  };
  const pointers = new Map();

  function dpadBits(e) {
    const r = dpad.getBoundingClientRect();
    const dx = (e.clientX - r.left) / r.width - 0.5;
    const dy = (e.clientY - r.top) / r.height - 0.5;
    if (Math.hypot(dx, dy) < 0.12) return 0;
    let bits = 0;
    // 0.20 rather than 0.5/sqrt(2): a generous diagonal zone beats a precise
    // one on glass, where thumbs drift toward the cardinal they started on.
    if (dx > 0.2) bits |= 16;
    else if (dx < -0.2) bits |= 32;
    if (dy < -0.2) bits |= 64;
    else if (dy > 0.2) bits |= 128;
    return bits;
  }

  function applyDpad() {
    let bits = 0;
    for (const b of pointers.values()) bits |= b;
    touchPressed = (touchPressed & ~DPAD_MASK) | bits;
    for (const [bit, el] of Object.entries(arms)) {
      el.classList.toggle("pressed", (bits & bit) !== 0);
    }
  }

  dpad.addEventListener("pointerdown", (e) => {
    try {
      dpad.setPointerCapture(e.pointerId);
    } catch {
      // A pointer that vanished between the event and here is already gone.
    }
    pointers.set(e.pointerId, dpadBits(e));
    applyDpad();
    e.preventDefault();
  });
  dpad.addEventListener("pointermove", (e) => {
    if (!pointers.has(e.pointerId)) return;
    pointers.set(e.pointerId, dpadBits(e));
    applyDpad();
  });
  for (const type of ["pointerup", "pointercancel"]) {
    dpad.addEventListener(type, (e) => {
      pointers.delete(e.pointerId);
      applyDpad();
    });
  }
  dpad.addEventListener("contextmenu", (e) => e.preventDefault());
}

if (isTouch) setupTouch();

window.addEventListener("keydown", (e) => key(e, true));
window.addEventListener("keyup", (e) => key(e, false));
window.addEventListener("blur", () => {
  pressed = 0;
});

async function startAudio(sampleRate) {
  if (audioCtx) {
    await audioCtx.resume();
    return;
  }
  audioCtx = new AudioContext({ sampleRate });
  await audioCtx.audioWorklet.addModule("./audio-worklet.js");
  sink = new AudioWorkletNode(audioCtx, "gba-sink", {
    numberOfInputs: 0,
    numberOfOutputs: 1,
    outputChannelCount: [2],
  });
  sink.port.onmessage = (e) => {
    backlog = e.data;
  };
  sink.connect(audioCtx.destination);
}

// Battery saves have nowhere to go in a browser tab, so they are mirrored into
// IndexedDB and restored the next time the same cartridge is loaded. IndexedDB
// rather than local storage because a flash save is 128KB of binary, which only
// fits in local storage after a base64 expansion that eats most of its quota.
const DB_NAME = "gba";
const STORE = "saves";
let dbPromise = null;

function openDb() {
  if (dbPromise) return dbPromise;
  dbPromise = new Promise((resolve, reject) => {
    const req = indexedDB.open(DB_NAME, 1);
    req.onupgradeneeded = () => req.result.createObjectStore(STORE);
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
  return dbPromise;
}

async function dbGet(key) {
  const db = await openDb();
  return new Promise((resolve, reject) => {
    const req = db.transaction(STORE, "readonly").objectStore(STORE).get(key);
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

async function dbPut(key, value) {
  const db = await openDb();
  return new Promise((resolve, reject) => {
    const tx = db.transaction(STORE, "readwrite");
    tx.objectStore(STORE).put(value, key);
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error);
  });
}

// FNV-1a over the ROM, so two dumps that share a header title (a game and its
// romhack, say) still get separate saves.
function romKey(bytes, title) {
  let h = 0x811c9dc5;
  for (let i = 0; i < bytes.length; i += 64) {
    h ^= bytes[i];
    h = Math.imul(h, 0x01000193) >>> 0;
  }
  h ^= bytes.length;
  h = Math.imul(h, 0x01000193) >>> 0;
  return `${title}:${h.toString(16)}`;
}

let writing = false;

async function persistSave() {
  if (!emu || !saveKey || writing || !emu.save_dirty()) return;
  const bytes = emu.save_data();
  if (bytes.length === 0) return;
  writing = true;
  try {
    await dbPut(saveKey, bytes);
  } catch {
    // A failed write is not worth interrupting play for; the next tick retries.
  } finally {
    writing = false;
  }
}

async function loadRom(bytes, name) {
  await persistSave();
  let next;
  try {
    next = new Emulator(bytes);
  } catch (err) {
    status(`could not load ${name}: ${err}`);
    return;
  }
  // Restore before the loop can see the new machine, so the game never boots
  // against empty save memory and then has it swapped underneath it.
  emu = null;
  romName = next.title().trim() || name;
  saveKey = romKey(bytes, romName);
  try {
    const stored = await dbGet(saveKey);
    if (stored && stored.length > 0) next.load_save(new Uint8Array(stored));
  } catch {
    // A missing or corrupt entry just means the game starts fresh.
  }
  emu = next;
  if (sink) sink.port.postMessage("flush");
  status(running ? `running ${romName}` : `${romName} ready, press start`);
}

let sinceSave = 0;

// The GBA draws 16777216 / 280896 frames a second. requestAnimationFrame runs
// at the display's refresh rate instead, which is 120Hz on a ProMotion screen
// and would otherwise play everything at double speed, so emulated frames are
// paced against the wall clock rather than against the callback.
const FRAME_MS = 1000 * 280896 / 16777216;
let owed = 0;
let last = 0;
let stepped = 0;

function frame(now) {
  requestAnimationFrame(frame);
  if (!emu || !running) {
    last = now;
    return;
  }

  const elapsed = last ? now - last : FRAME_MS;
  last = now;
  // Cap the backlog at four frames. A backgrounded tab stops getting callbacks
  // entirely, and without this the first callback after it returns would try to
  // emulate every missed frame at once and never catch up.
  owed = Math.min(owed + elapsed / FRAME_MS, 4);

  emu.set_keys(pressed | touchPressed);
  while (owed >= 1) {
    owed -= 1;
    emu.step_frame();
    stepped++;
  }
  image.data.set(emu.framebuffer());
  ctx.putImageData(image, 0, 0);

  if (sink) {
    // Keep at most half a second of stereo audio buffered; beyond that the
    // page has drifted ahead and the extra samples would only add latency.
    const samples = emu.take_audio();
    if (backlog > audioCtx.sampleRate / 2) {
      sink.port.postMessage("flush");
    } else if (samples.length > 0) {
      sink.port.postMessage(samples, [samples.buffer]);
    }
  } else {
    emu.clear_audio();
  }

  // Flush the battery save at most once a second, and only when the cartridge
  // actually touched it.
  if (now - sinceSave >= 1000) {
    sinceSave = now;
    persistSave();
  }
}

async function fetchTestRom() {
  status("fetching arm.gba from jsmolka/gba-tests");
  try {
    const res = await fetch(TEST_ROM);
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    await loadRom(new Uint8Array(await res.arrayBuffer()), "arm.gba");
  } catch (err) {
    status(`could not fetch the test ROM: ${err}`);
  }
}

async function main() {
  // The service worker exists for the installed home-screen copy: it caches
  // the shell so the app opens offline. file:// and localhost dev skip it.
  if ("serviceWorker" in navigator && location.protocol === "https:") {
    navigator.serviceWorker.register("sw.js").catch(() => {});
  }
  await init();
  status("no ROM loaded");
  requestAnimationFrame(frame);

  startBtn.addEventListener("click", async () => {
    if (emu) await startAudio(emu.sample_rate());
    overlay.classList.add("hidden");
    owed = 0;
    last = 0;
    running = true;
    status(romName ? `running ${romName}` : "pick a ROM to start");
  });

  demoBtn.addEventListener("click", fetchTestRom);

  romInput.addEventListener("change", async () => {
    const file = romInput.files[0];
    if (!file) return;
    await loadRom(new Uint8Array(await file.arrayBuffer()), file.name);
  });

  // A tab can be closed or hidden without ever running another frame, so flush
  // on the way out as well as on the once-a-second timer.
  window.addEventListener("pagehide", persistSave);
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "hidden") persistSave();
  });
  // Exposed so the pacing can be checked from the console: read it, wait a
  // known number of seconds, read it again, and the difference should be about
  // 59.7 emulated frames per second regardless of the display's refresh rate.
  Object.defineProperty(window, "gbaFramesStepped", { get: () => stepped });
}

main().catch((err) => status(`failed to start: ${err}`));
