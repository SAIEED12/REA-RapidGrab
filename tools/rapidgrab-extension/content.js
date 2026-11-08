// RapidGrab content script: media sniffing + player download button.
// Runs on every page (all_frames, document_start). Finds <video>/<audio>
// elements and <a download> / *.m3u8 links, and shows one small
// "↓ RapidGrab" button per player (IDM player-panel equivalent). The button
// is always visible while its player is in view. Clicking sends the media
// URL to the background, which captures it as source "media". Never modifies
// the page's own controls.
//
// Only real players get a button: thumbnail previews (YouTube homepage,
// search results) are <video> elements too, but attaching to them produced
// a full-width black bar artifact. A player must be large and not be a link
// thumbnail.
//
// The button is draggable: it lives at the page level (position: fixed in
// document.body, or inside the fullscreen element while a player is
// fullscreen) so it can be moved off the player's own controls, and its
// offset is remembered per site in localStorage. It stays anchored to its
// player while scrolling, and is pointer-inert while hidden so it never
// swallows clicks meant for the page.

(() => {
  const api = globalThis.browser || globalThis.chrome;
  const BTN_ID = "rapidgrab-btn";
  const BTN_TEXT = "↓ RapidGrab";
  const MIN_PLAYER_W = 480;
  const MIN_PLAYER_H = 200;
  const POS_KEY = "rapidgrab.btnPos";
  const DRAG_THRESHOLD = 4;
  const DEFAULT_MARGIN = 8;

  function mediaUrlFor(el) {
    if (!el) return null;
    // YouTube and most players use MSE: currentSrc is a blob: URL that
    // cannot be downloaded. Fall back to the page URL (the resolver
    // handles it) rather than sending an undownloadable blob:.
    const candidates = [];
    if (el.tagName === "VIDEO" || el.tagName === "AUDIO") {
      if (el.currentSrc) candidates.push(el.currentSrc);
      const src = el.querySelector("source[src]");
      if (src && src.src) candidates.push(src.src);
      if (el.src) candidates.push(el.src);
    }
    for (const candidate of candidates) {
      if (/^https?:\/\//i.test(candidate)) return candidate;
    }
    return null;
  }

  function titleFor() {
    return document.title || "";
  }

  function isRealPlayer(video) {
    if (!video || video.tagName !== "VIDEO") return false;
    // Thumbnail previews are wrapped in a link; real players are not.
    if (video.closest("a[href]")) return false;
    const rect = video.getBoundingClientRect();
    if (rect.width < MIN_PLAYER_W || rect.height < MIN_PLAYER_H) return false;
    return true;
  }

  function loadOffset() {
    try {
      const raw = localStorage.getItem(POS_KEY);
      if (!raw) return null;
      const value = JSON.parse(raw);
      if (typeof value.dx === "number" && typeof value.dy === "number") {
        return { dx: value.dx, dy: value.dy };
      }
    } catch (_) {}
    return null;
  }

  function saveOffset(offset) {
    try {
      localStorage.setItem(POS_KEY, JSON.stringify(offset));
    } catch (_) {}
  }

  function clamp(value, min, max) {
    return Math.min(Math.max(value, min), max);
  }

  function ensureButton(video) {
    if (!video || video.dataset.rgBtn) return;
    if (!isRealPlayer(video)) return;
    video.dataset.rgBtn = "1";

    const btn = document.createElement("button");
    btn.id = BTN_ID + "-" + Math.random().toString(36).slice(2, 8);
    btn.className = "rg-fab";
    btn.textContent = BTN_TEXT;
    btn.title = "Drag to move · Click to download";
    btn.type = "button";
    btn.style.cssText = [
      "position:fixed", "left:0", "top:0", "z-index:2147483647",
      "background:#1b1b1b", "color:#60cdff", "border:1px solid #3a3a3a",
      "border-radius:6px", "padding:4px 8px", "font:12px system-ui, sans-serif",
      "cursor:default", "opacity:0", "transition:opacity .15s", "pointer-events:none",
      "white-space:nowrap", "width:auto", "max-width:160px",
      "touch-action:none", "user-select:none", "-webkit-user-select:none",
      "-webkit-user-drag:none",
    ].join(";");

    // Position is an offset from the player's top-left; null = default
    // top-right corner of the player.
    let offset = loadOffset();
    let onScreen = true;
    let shown = false;
    let dragging = false;
    let moved = false;
    let suppressClick = false;
    let raf = 0;
    let drag = null;
    let disposed = false;

    (document.body || document.documentElement).appendChild(btn);

    function hostElement() {
      return document.fullscreenElement || document.body || document.documentElement;
    }

    function applyPosition() {
      raf = 0;
      if (disposed) return;
      const host = hostElement();
      if (btn.parentElement !== host) host.appendChild(btn);
      const rect = video.getBoundingClientRect();
      const width = btn.offsetWidth;
      const height = btn.offsetHeight;
      const baseLeft = offset
        ? rect.left + offset.dx
        : rect.right - width - DEFAULT_MARGIN;
      const baseTop = offset ? rect.top + offset.dy : rect.top + DEFAULT_MARGIN;
      const left = clamp(baseLeft, 0, Math.max(0, window.innerWidth - width));
      const top = clamp(baseTop, 0, Math.max(0, window.innerHeight - height));
      btn.style.left = left + "px";
      btn.style.top = top + "px";
    }

    function scheduleLayout() {
      if (raf || disposed) return;
      raf = requestAnimationFrame(applyPosition);
    }

    function forceHide() {
      shown = false;
      btn.style.opacity = "0";
      btn.style.pointerEvents = "none";
    }

    // Always-visible badge: no hover gating. It only hides while its player
    // is out of view (see the IntersectionObserver below).
    function show() {
      if (disposed || !onScreen) return;
      applyPosition();
      if (shown) return;
      shown = true;
      btn.style.opacity = "1";
      btn.style.pointerEvents = "auto";
    }

    // --- drag ------------------------------------------------------------
    btn.addEventListener("pointerdown", (event) => {
      if (event.button !== 0) return;
      // Keep the page (player click-to-pause, menus, etc.) out of the drag.
      event.stopPropagation();
      show();
      const rect = video.getBoundingClientRect();
      const box = btn.getBoundingClientRect();
      dragging = true;
      moved = false;
      suppressClick = false;
      drag = {
        pointerId: event.pointerId,
        startX: event.clientX,
        startY: event.clientY,
        startDx: box.left - rect.left,
        startDy: box.top - rect.top,
      };
      try {
        btn.setPointerCapture(event.pointerId);
      } catch (_) {}
    });

    btn.addEventListener("pointermove", (event) => {
      if (!dragging || !drag || event.pointerId !== drag.pointerId) return;
      event.preventDefault();
      event.stopPropagation();
      if (
        !moved &&
        Math.hypot(event.clientX - drag.startX, event.clientY - drag.startY) >
          DRAG_THRESHOLD
      ) {
        moved = true;
      }
      offset = {
        dx: drag.startDx + (event.clientX - drag.startX),
        dy: drag.startDy + (event.clientY - drag.startY),
      };
      scheduleLayout();
    });

    function endDrag(event) {
      if (!dragging || !drag) return;
      dragging = false;
      if (event && event.pointerId === drag.pointerId) {
        // No preventDefault here: a plain click must still reach the click
        // handler and send the capture.
        event.stopPropagation();
        try {
          btn.releasePointerCapture(event.pointerId);
        } catch (_) {}
      }
      drag = null;
      if (moved && offset) saveOffset(offset);
      // A drop must never be mistaken for a click (no surprise download).
      suppressClick = moved;
      moved = false;
    }
    btn.addEventListener("pointerup", endDrag);
    btn.addEventListener("pointercancel", endDrag);

    btn.addEventListener("click", (event) => {
      event.stopPropagation();
      event.preventDefault();
      if (suppressClick) {
        suppressClick = false;
        return;
      }
      // Empty when the player is MSE (blob:): the service worker resolves
      // it to the most recent media response it saw for this tab.
      const url = mediaUrlFor(video) || "";
      api.runtime
        .sendMessage({
          type: "rapidgrab-media",
          url,
          title: titleFor(),
        })
        .catch(() => {});
    });

    // --- size gate -------------------------------------------------------
    // A preview that grows into a player should still not get a button —
    // isRealPlayer gates that — but a real player the user resized should
    // hide/show correctly.
    let scheduled = false;
    const place = () => {
      if (scheduled) return;
      scheduled = true;
      requestAnimationFrame(() => {
        scheduled = false;
        if (disposed) return;
        try {
          const real = isRealPlayer(video);
          btn.style.display = real ? "" : "none";
          if (real) scheduleLayout();
        } catch (_) {}
      });
    };
    const resizeObserver = new ResizeObserver(place);
    resizeObserver.observe(video);

    // Hide while the player is scrolled out of view so the fixed button
    // never floats over unrelated content.
    let intersectionObserver = null;
    if ("IntersectionObserver" in window) {
      intersectionObserver = new IntersectionObserver((entries) => {
        onScreen = entries.some((entry) => entry.isIntersecting);
        if (onScreen) {
          // Come back automatically; no mouse movement required.
          show();
          scheduleLayout();
        } else {
          dragging = false;
          drag = null;
          forceHide();
        }
      });
      intersectionObserver.observe(video);
    }

    const onScroll = () => scheduleLayout();
    window.addEventListener("scroll", onScroll, { capture: true, passive: true });
    window.addEventListener("resize", onScroll, { passive: true });
    document.addEventListener("fullscreenchange", scheduleLayout);
    show();

    // --- teardown (player removed by SPA navigation) ---------------------
    btn._rgVideo = video;
    btn._rgTeardown = () => {
      disposed = true;
      window.removeEventListener("scroll", onScroll, true);
      window.removeEventListener("resize", onScroll);
      document.removeEventListener("fullscreenchange", scheduleLayout);
      if (raf) cancelAnimationFrame(raf);
      if (intersectionObserver) intersectionObserver.disconnect();
      resizeObserver.disconnect();
      delete video.dataset.rgBtn;
      btn.remove();
    };
  }

  function scan() {
    document.querySelectorAll("video").forEach(ensureButton);
    // Drop buttons whose player element is gone (SPA navigation).
    document.querySelectorAll("button.rg-fab").forEach((btn) => {
      const video = btn._rgVideo;
      if (!video || !video.isConnected) {
        if (typeof btn._rgTeardown === "function") btn._rgTeardown();
        else btn.remove();
      }
    });
  }

  const observer = new MutationObserver(scan);
  observer.observe(document.documentElement, { childList: true, subtree: true });
  scan();
})();
