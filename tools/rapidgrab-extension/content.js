// RapidGrab content script: media sniffing + player download button.
// Runs on every page (all_frames, document_start). Finds <video>/<audio>
// elements and <a download> / *.m3u8 links, and shows one small
// "↓ RapidGrab" button per player on hover (IDM player-panel equivalent).
// Clicking sends the media URL to the background, which captures it as
// source "media". Never modifies the page's own controls.
//
// Only real players get a button: thumbnail previews (YouTube homepage,
// search results) are <video> elements too, but attaching to them produced
// a full-width black bar artifact. A player must be large, not be a link
// thumbnail, and the button is positioned inside the video's wrapper
// (absolute, not fixed) so it can never stretch across the page.

(() => {
  const api = globalThis.browser || globalThis.chrome;
  const BTN_ID = "rapidgrab-btn";
  const BTN_TEXT = "↓ RapidGrab";
  const MIN_PLAYER_W = 480;
  const MIN_PLAYER_H = 200;

  function mediaUrlFor(el) {
    if (!el) return null;
    if (el.tagName === "VIDEO" || el.tagName === "AUDIO") {
      if (el.currentSrc) return el.currentSrc;
      const src = el.querySelector("source[src]");
      if (src && src.src) return src.src;
      if (el.src) return el.src;
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

  function ensureButton(video) {
    if (!video || video.dataset.rgBtn) return;
    if (!isRealPlayer(video)) return;
    video.dataset.rgBtn = "1";

    const parent = video.parentElement || document.body;
    const parentStyle = getComputedStyle(parent);
    if (parentStyle.position === "static") parent.style.position = "relative";

    const btn = document.createElement("button");
    btn.id = BTN_ID + "-" + Math.random().toString(36).slice(2, 8);
    btn.textContent = BTN_TEXT;
    btn.title = "Download with RapidGrab";
    btn.type = "button";
    btn.style.cssText = [
      "position:absolute", "top:8px", "right:8px", "z-index:2147483647",
      "background:#1b1b1b", "color:#60cdff", "border:1px solid #3a3a3a",
      "border-radius:6px", "padding:4px 8px", "font:12px system-ui, sans-serif",
      "cursor:pointer", "opacity:0", "transition:opacity .15s", "pointer-events:auto",
      "white-space:nowrap", "width:auto", "max-width:160px",
    ].join(";");
    btn.addEventListener("mouseenter", () => {
      btn.style.opacity = "1";
    });
    btn.addEventListener("mouseleave", () => {
      btn.style.opacity = "0";
    });
    btn.addEventListener("click", (event) => {
      event.stopPropagation();
      event.preventDefault();
      const url = mediaUrlFor(video);
      if (!url) return;
      api.runtime
        .sendMessage({
          type: "rapidgrab-media",
          url,
          title: titleFor(),
        })
        .catch(() => {});
    });
    parent.appendChild(btn);

    // Re-check size on hover (a preview that grows into a player should
    // still not get a button — isRealPlayer gates that — but a real player
    // the user resized should hide/show correctly).
    let scheduled = false;
    const place = () => {
      if (scheduled) return;
      scheduled = true;
      requestAnimationFrame(() => {
        scheduled = false;
        try {
          btn.style.display = isRealPlayer(video) ? "" : "none";
        } catch (_) {}
      });
    };
    video.addEventListener("mouseenter", () => {
      btn.style.opacity = "1";
    });
    video.addEventListener("mouseleave", () => {
      btn.style.opacity = "0";
    });
    new ResizeObserver(place).observe(video);
  }

  function scan() {
    document.querySelectorAll("video").forEach(ensureButton);
  }

  const observer = new MutationObserver(scan);
  observer.observe(document.documentElement, { childList: true, subtree: true });
  scan();
})();
