// The tour of the app. Both renderings are in the HTML; CSS picks one (see
// tour.module.css): the scrubbed animation, or static scenes for reduced motion
// and for visitors without scripts.

import { AnimatedTour } from "./animated-tour";
import { AppWindow } from "./app-window";
import { TOUR_SCENES } from "./tour-scenes";
import s from "./tour.module.css";

export function Tour() {
  return (
    <section className={s.tour} aria-label="A tour of the app">
      <AnimatedTour />
      <div className={s.static}>
        {TOUR_SCENES.map((scene, i) => (
          <div key={scene.id} className={s.scene}>
            <div className={s.caption}>
              <h2 className={s.captionTitle}>{scene.title}</h2>
              <p className={s.captionBody}>{scene.body}</p>
            </div>
            <AppWindow
              view={scene.view}
              download={scene.download}
              label={scene.windowLabel}
              priority={i === 0}
            />
          </div>
        ))}
      </div>
    </section>
  );
}
