import { type RefObject, useEffect, useRef } from "react";

/**
 * Runs `callback` every `ms` while `ref` is on screen. Pass `null` to stop.
 * Does nothing for visitors who prefer reduced motion.
 */
export function useVisibleInterval(
  ref: RefObject<Element>,
  ms: number | null,
  callback: () => void,
  threshold = 0,
) {
  const latest = useRef(callback);
  latest.current = callback;

  useEffect(() => {
    const element = ref.current;
    if (ms === null || !element) return;
    if (matchMedia("(prefers-reduced-motion: reduce)").matches) return;

    let timer: ReturnType<typeof setInterval> | undefined;
    const observer = new IntersectionObserver(
      ([entry]) => {
        clearInterval(timer);
        if (entry.isIntersecting) {
          timer = setInterval(() => latest.current(), ms);
        }
      },
      { threshold },
    );
    observer.observe(element);

    return () => {
      clearInterval(timer);
      observer.disconnect();
    };
  }, [ref, ms, threshold]);
}
