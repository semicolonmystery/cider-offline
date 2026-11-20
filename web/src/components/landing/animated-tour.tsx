"use client";

// The scroll-driven tour: one sticky stage, one GSAP timeline scrubbed by native
// scrolling. Runs only when motion is allowed; otherwise tour.tsx shows the static
// scenes and this subtree is hidden by CSS.

import { useRef } from "react";
import gsap from "gsap";
import { ScrollTrigger } from "gsap/ScrollTrigger";
import { useGSAP } from "@gsap/react";

import { AppWindow } from "./app-window";
import { TOUR_SCENES } from "./tour-scenes";
import s from "./tour.module.css";

gsap.registerPlugin(useGSAP, ScrollTrigger);

/** Position of `el` inside `root` in CSS pixels, ignoring transforms. */
function offsetWithin(el: HTMLElement, root: HTMLElement) {
  let x = 0;
  let y = 0;
  let node: HTMLElement | null = el;
  while (node && node !== root) {
    x += node.offsetLeft;
    y += node.offsetTop;
    node = node.offsetParent as HTMLElement | null;
  }
  return { x, y };
}

/** Centre of `el` inside `root`. */
function centreWithin(el: HTMLElement, root: HTMLElement) {
  const { x, y } = offsetWithin(el, root);
  return { x: x + el.offsetWidth / 2, y: y + el.offsetHeight / 2 };
}

function buildTimeline(root: HTMLElement, wide: boolean) {
  const q = (name: string) =>
    Array.from(root.querySelectorAll<HTMLElement>(`[data-anim="${name}"]`));
  const one = (name: string) => q(name)[0];

  const tour = root;
  const stage = one("stage");
  const slot = one("slot");
  const win = one("window");
  const library = one("library");
  const detail = one("detail");
  const card = one("open-card");
  const cursor = one("cursor");
  const albumControl = one("album-control");
  const rowControls = q("row-control");
  const rows = q("row");
  const captions = q("caption");
  if (!stage || !slot || !win || !card || !albumControl || !cursor) return;

  const dimAndChip = card.querySelectorAll('[data-anim="cover-dim"], [data-anim="more-chip"]');
  // App pixels to CSS pixels.
  const px = () => win.offsetWidth / (parseFloat(getComputedStyle(win).getPropertyValue("--dw")) || 1200);
  // How far the window steps aside on wide screens, so its edge meets the gutter.
  const gutter = () => parseFloat(getComputedStyle(stage).paddingLeft) || 0;
  const shift = () => (wide ? (stage.clientWidth - win.offsetWidth) / 2 - gutter() : 0);
  const from = (side: string) => (side === "left" ? -48 : 48);

  // The cursor's tip sits at its top-left corner.
  const cursorAt = (target: HTMLElement) => () => centreWithin(target, win);
  const cardPoint = cursorAt(card.querySelector<HTMLElement>("img") ?? card);
  const controlPoint = cursorAt(albumControl);

  gsap.set(cursor, { x: () => win.offsetWidth * 0.7, y: () => win.offsetHeight * 0.85 });

  const tl = gsap.timeline({
    defaults: { ease: "power2.inOut" },
    scrollTrigger: {
      trigger: tour,
      start: "top top",
      end: "bottom bottom",
      scrub: 0.6,
      invalidateOnRefresh: true,
    },
  });

  const [cap1, cap2, cap3] = captions;
  const side = (el: HTMLElement) => el.dataset.side ?? "left";

  // Scene 1: the window steps right, the library caption comes in from the left.
  tl.to(slot, { x: () => shift(), duration: 1 })
    .fromTo(cap1, { autoAlpha: 0, x: from(side(cap1)) }, { autoAlpha: 1, x: 0, duration: 0.8 }, "<0.2")
    .to({}, { duration: 0.8 });

  // Scene 2: open Graduation.
  tl.to(cap1, { autoAlpha: 0, x: from(side(cap1)), duration: 0.5 })
    .to(slot, { x: () => -shift(), duration: 1 }, "<")
    .fromTo(cap2, { autoAlpha: 0, x: from(side(cap2)) }, { autoAlpha: 1, x: 0, duration: 0.8 }, "<0.3")
    .to(cursor, { autoAlpha: 1, duration: 0.2 }, "<")
    .to(cursor, { x: () => cardPoint().x, y: () => cardPoint().y, duration: 0.9, ease: "power2.out" })
    .to(dimAndChip, { autoAlpha: 1, duration: 0.15 })
    .to(card, { scale: 0.96, duration: 0.12, ease: "power1.out" })
    .to(card, { scale: 1, duration: 0.12, ease: "power1.out" })
    .to(library, { x: () => -40 * px(), autoAlpha: 0, duration: 0.6 })
    .fromTo(
      detail,
      { x: () => 40 * px(), autoAlpha: 0 },
      { x: 0, autoAlpha: 1, duration: 0.6 },
      "<",
    )
    .to(cursor, { autoAlpha: 0, duration: 0.2 }, "<")
    .from(rows, { autoAlpha: 0, y: () => 8 * px(), stagger: 0.05, duration: 0.3, ease: "power2.out" }, "<0.25")
    .to({}, { duration: 0.8 });

  // Scene 3: download the album, two songs at a time.
  tl.to(cap2, { autoAlpha: 0, x: from(side(cap2)), duration: 0.5 })
    .to(slot, { x: () => shift(), duration: 1 }, "<")
    .fromTo(cap3, { autoAlpha: 0, x: from(side(cap3)) }, { autoAlpha: 1, x: 0, duration: 0.8 }, "<0.3")
    .set(cursor, { x: () => win.offsetWidth * 0.6, y: () => win.offsetHeight * 0.75 }, "<")
    .to(cursor, { autoAlpha: 1, duration: 0.2 }, "<")
    .to(cursor, { x: () => controlPoint().x, y: () => controlPoint().y, duration: 0.8, ease: "power2.out" })
    .to(albumControl, { scale: 0.96, duration: 0.12, ease: "power1.out" })
    .to(albumControl, { scale: 1, duration: 0.12, ease: "power1.out" })
    .set(albumControl, { attr: { "data-state": "downloading" } }, "<")
    .set(rowControls, { attr: { "data-state": "queued" } }, "<")
    .to(cursor, { autoAlpha: 0, duration: 0.3 });

  const start = tl.duration();
  const slotLength = 0.5;
  rowControls.forEach((control, i) => {
    const at = start + Math.floor(i / 2) * slotLength;
    tl.set(control, { attr: { "data-state": "downloading" } }, at)
      .fromTo(control, { "--p": 0 }, { "--p": 1, duration: slotLength * 0.9, ease: "none" }, at)
      .set(control, { attr: { "data-state": "done" } }, at + slotLength);
  });
  const songsTime = Math.ceil(rowControls.length / 2) * slotLength;
  tl.fromTo(albumControl, { "--p": 0 }, { "--p": 1, duration: songsTime, ease: "none" }, start)
    .set(albumControl, { attr: { "data-state": "done" } }, start + songsTime)
    .to({}, { duration: 0.8 });
}

export function AnimatedTour() {
  const root = useRef<HTMLDivElement>(null);

  useGSAP(
    () => {
      const mm = gsap.matchMedia();
      mm.add(
        {
          wide: "(min-width: 1200px)",
          motion: "(prefers-reduced-motion: no-preference)",
        },
        (context) => {
          const { wide, motion } = context.conditions as { wide: boolean; motion: boolean };
          if (motion && root.current) buildTimeline(root.current, wide);
        },
        root,
      );
      return () => mm.revert();
    },
    { scope: root },
  );

  return (
    <div className={s.animated} data-anim="tour" ref={root}>
      <div className={s.stage} data-anim="stage">
        <div className={s.windowSlot} data-anim="slot">
          <AppWindow
            view="library"
            launch
            priority
            label="The Cider Offline window. It follows the captions: the Library page, then the album Graduation, then its download."
          />
        </div>
        <div className={s.captions}>
          {TOUR_SCENES.map((scene) => (
            <div key={scene.id} className={s.caption} data-side={scene.side} data-anim="caption">
              <h2 className={s.captionTitle}>{scene.title}</h2>
              <p className={s.captionBody}>{scene.body}</p>
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}
