// RapidGrab content script: media sniffing + player download button.
// Runs on every page (all_frames, document_start). Finds <video>/<audio>
// elements and <a download> / *.m3u8 links, and shows one small
// "↓ RapidGrab" button per player on hover (IDM player-panel equivalent).
// Clicking sends the media URL to the background, which captures it as
// source "media". Never modifies the page's own controls.

(() => {
  const api = globalThis.browser || globalThis.chrome;
  const BTN_ID = "rapidgrab-btn";
  const BTN_TEXT = "↓ RapidGrab";

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

  function ensureButton(video) {
    if (!video || video.dataset.rgBtn) return;
    video.dataset.rgBtn = "1";
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

    // Attach into the video's positioning context.
    const style = getComputedStyle(video);
    if (style.position === "static") video.style.position = "relative";
    document.body.appendChild(btn);
    const place = () => {
      try {
        const rect = video.getBoundingClientRect();
        if (rect.width < 80 || rect.height < 40) {
          btn.style.display = "none";
          return;
        }
        btn.style.display = "";
        btn.style.left = rect.right - btn.offsetWidth - 8 + "px";
        btn.style.top = rect.top + 8 + "px";
        btn.style.position = "fixed";
      } catch (_) {}
    };
    video.addEventListener("mouseenter", () => {
      btn.style.opacity = "1";
      place();
    });
    video.addEventListener("mouseleave", () => {
      btn.style.opacity = "0";
    });
    new ResizeObserver(place).observe(video);
    place();
  }

  function scan() {
    document.querySelectorAll("video").forEach(ensureButton);
  }

  const observer = new MutationObserver(scan);
  observer.observe(document.documentElement, { childList: true, subtree: true });
  scan();
})();
