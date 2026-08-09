/**
 * Splits latency points into streaming and non-streaming chart datasets.
 *
 * Streaming rows measure time-to-first-token (`ttft_ms`); non-streaming rows measure
 * end-to-end completion time (`total_ms`). They are different quantities, so they get
 * separate datasets and point styles — plotting them alike would read a 90-second
 * completion as a 90-second TTFT.
 *
 * Shared by TtftChart.vue (per server) and PublicUsagePage.vue (per model).
 */

export interface LatencyPoint {
  created_at: string;
  /** Set only for streaming rows. */
  ttft_ms: number | null;
  /** Set only for non-streaming rows. */
  total_ms: number | null;
  timed_out: boolean;
  is_streaming: boolean;
}

export interface XYPoint {
  x: number;
  y: number;
}

export interface LatencyDatasets {
  /** Streaming points, per-point styled so timeouts stand out. */
  stream: {
    data: XYPoint[];
    backgroundColor: string[];
    pointRadius: number[];
    pointStyle: string[];
  };
  /** Non-streaming points, uniformly styled as triangles. */
  nonStream: {
    data: XYPoint[];
  };
}

/** Timeouts are plotted at y=0 in this colour so they read as failures, not fast replies. */
export const TIMEOUT_COLOR = '#EF5350';

/**
 * Bucket one series of points into the two datasets.
 *
 * A timed-out point has no latency to plot, so it lands at y=0 on whichever side it
 * belongs to. A point with no value on its own side is dropped: a streaming row with a
 * null `ttft_ms` carries no TTFT, and inventing one would corrupt the chart.
 */
export function splitLatencyPoints(points: LatencyPoint[], color: string): LatencyDatasets {
  const result: LatencyDatasets = {
    stream: { data: [], backgroundColor: [], pointRadius: [], pointStyle: [] },
    nonStream: { data: [] },
  };

  for (const p of points) {
    const x = new Date(p.created_at).getTime();
    const value = p.is_streaming ? p.ttft_ms : p.total_ms;

    if (p.timed_out) {
      if (p.is_streaming) {
        result.stream.data.push({ x, y: 0 });
        result.stream.backgroundColor.push(TIMEOUT_COLOR);
        result.stream.pointRadius.push(6);
        result.stream.pointStyle.push('crossRot');
      } else {
        result.nonStream.data.push({ x, y: 0 });
      }
      continue;
    }

    if (value == null) continue;

    if (p.is_streaming) {
      result.stream.data.push({ x, y: value });
      result.stream.backgroundColor.push(color);
      result.stream.pointRadius.push(4);
      result.stream.pointStyle.push('circle');
    } else {
      result.nonStream.data.push({ x, y: value });
    }
  }

  return result;
}
