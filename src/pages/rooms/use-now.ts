import { useEffect, useState } from "react";

/** 每隔 `intervalMs` 刷新一次的当前时间，用于已录时长和相对时间。 */
export function useNow(intervalMs = 1000): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), intervalMs);
    return () => clearInterval(timer);
  }, [intervalMs]);
  return now;
}
