// DOM side of the sticky modifiers (modifier-latch.js): the Ctrl/Alt/Shift/Win buttons in the Keys
// popover and in the bar above the soft keyboard, the strip's latch label and the bar's position.
// A tap latches for one key, a second tap or a long press locks, another tap unlocks.

const VK_CTRL = 162, VK_ALT = 164, VK_SHIFT = 160, VK_WIN = 91;
const NAMES = new Map([[VK_CTRL, "Ctrl"], [VK_ALT, "Alt"], [VK_SHIFT, "Shift"], [VK_WIN, "Win"]]);

// env: latch (modifier-latch.js), ready() (input can be sent).
export function createModifierUi({ latch, ready }) {
  const held = () => [...NAMES].filter(([vk]) => latch.state(vk) !== "off");

  function render() {
    document.querySelectorAll(".modifier[data-vk]").forEach(button => {
      const vk = Number(button.dataset.vk);
      const state = latch.state(vk);
      button.classList.toggle("active", state !== "off");
      button.classList.toggle("locked", state === "locked");
      button.setAttribute("aria-pressed", String(state !== "off"));
      button.setAttribute("aria-label", state === "off" ? NAMES.get(vk) : `${NAMES.get(vk)}, ${state}`);
    });
    const marks = held().map(([vk, name]) => {
      const mark = document.createElement("span");
      mark.textContent = name[0];
      if (latch.state(vk) === "locked") mark.className = "locked";
      return mark;
    });
    document.getElementById("latch-label")?.replaceChildren(...marks);
    const spoken = held().map(([vk, name]) => latch.state(vk) === "locked" ? `${name} locked` : name);
    document.getElementById("strip-keys")?.setAttribute("aria-label", spoken.length ? `Keys, holding ${spoken.join("+")}` : "Keys");
    const status = document.getElementById("keyboard-status");
    if (status) status.textContent = spoken.join(" · ");
  }

  function bind() {
    document.querySelectorAll(".modifier[data-vk]").forEach(button => {
      const vk = Number(button.dataset.vk);
      // Keep the soft keyboard's focus: the bar must not close the keyboard it sits on.
      button.addEventListener("mousedown", event => event.preventDefault());
      button.addEventListener("touchstart", event => event.preventDefault(), { passive: false });
      button.addEventListener("contextmenu", event => event.preventDefault());
      button.addEventListener("pointerdown", event => { if (event.button === 0 && ready()) latch.pointerDown(vk); });
      button.addEventListener("pointerup", () => { if (ready()) latch.pointerUp(vk); else latch.pointerCancel(vk); });
      for (const type of ["pointercancel", "pointerleave"]) button.addEventListener(type, () => latch.pointerCancel(vk));
      // Keyboard activation (Enter/Space) taps.
      button.addEventListener("click", event => { if (event.detail === 0 && ready()) latch.tap(vk); });
    });
    window.visualViewport?.addEventListener("resize", syncKeyboardBar);
    window.visualViewport?.addEventListener("scroll", syncKeyboardBar);
  }

  /** The bar shows while the soft keyboard's field has focus; its bottom edge follows the visual viewport. */
  function syncKeyboardBar() {
    const open = document.activeElement === document.getElementById("keyboard-input");
    document.body.classList.toggle("keyboard-open", open);
    const vv = window.visualViewport;
    const inset = vv ? Math.max(0, Math.round(innerHeight - vv.offsetTop - vv.height)) : 0;
    document.documentElement.style.setProperty("--kb-inset", `${inset}px`);
  }

  return { render, bind, syncKeyboardBar };
}
