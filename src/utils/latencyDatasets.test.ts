import { describe, expect, it } from 'vitest';
import { splitLatencyPoints, TIMEOUT_COLOR, type LatencyPoint } from './latencyDatasets';

function point(overrides: Partial<LatencyPoint>): LatencyPoint {
  return {
    created_at: '2024-01-01T00:00:00Z',
    ttft_ms: null,
    total_ms: null,
    timed_out: false,
    is_streaming: true,
    ...overrides,
  };
}

describe('splitLatencyPoints', () => {
  it('reads ttft_ms for a streaming point, ignoring total_ms', () => {
    const p = point({ is_streaming: true, ttft_ms: 150, total_ms: 99999 });
    const result = splitLatencyPoints([p], '#000');
    expect(result.stream.data).toEqual([{ x: Date.parse(p.created_at), y: 150 }]);
    expect(result.nonStream.data).toEqual([]);
  });

  it('reads total_ms for a non-streaming point, ignoring ttft_ms', () => {
    const p = point({ is_streaming: false, total_ms: 90000, ttft_ms: 12345 });
    const result = splitLatencyPoints([p], '#000');
    expect(result.nonStream.data).toEqual([{ x: Date.parse(p.created_at), y: 90000 }]);
    expect(result.stream.data).toEqual([]);
  });

  it('separates a mix of both kinds into their own datasets', () => {
    const streamA = point({ is_streaming: true, ttft_ms: 100 });
    const streamB = point({ is_streaming: true, ttft_ms: 300 });
    const nonStreamA = point({ is_streaming: false, total_ms: 70000 });
    const nonStreamB = point({ is_streaming: false, total_ms: 90000 });

    const result = splitLatencyPoints([streamA, nonStreamA, streamB, nonStreamB], '#123456');

    // Exactly the streaming values landed in `stream`, in order, nothing from non-stream.
    expect(result.stream.data.map((d) => d.y)).toEqual([100, 300]);
    // Exactly the non-streaming values landed in `nonStream`, in order.
    expect(result.nonStream.data.map((d) => d.y)).toEqual([70000, 90000]);
  });

  it('a 90000ms non-streaming completion never lands in the streaming dataset', () => {
    // This is the exact failure mode the split guards against: a long completion time
    // must never be readable as a slow TTFT.
    const slowCompletion = point({ is_streaming: false, total_ms: 90000 });
    const result = splitLatencyPoints([slowCompletion], '#000');
    expect(result.stream.data).toEqual([]);
    expect(result.nonStream.data).toEqual([{ x: Date.parse(slowCompletion.created_at), y: 90000 }]);
  });

  it('plots a streaming timeout at y=0 with the timeout color and marker', () => {
    const p = point({ is_streaming: true, timed_out: true, ttft_ms: null });
    const result = splitLatencyPoints([p], '#123456');
    expect(result.stream.data).toEqual([{ x: Date.parse(p.created_at), y: 0 }]);
    expect(result.stream.backgroundColor).toEqual([TIMEOUT_COLOR]);
    expect(result.stream.pointStyle).toEqual(['crossRot']);
  });

  it('plots a non-streaming timeout at y=0 without touching the streaming dataset', () => {
    const p = point({ is_streaming: false, timed_out: true, total_ms: null });
    const result = splitLatencyPoints([p], '#123456');
    expect(result.nonStream.data).toEqual([{ x: Date.parse(p.created_at), y: 0 }]);
    expect(result.stream.data).toEqual([]);
  });

  it('uses the given color and default styling for a normal streaming point', () => {
    const p = point({ is_streaming: true, ttft_ms: 200 });
    const result = splitLatencyPoints([p], '#abcdef');
    expect(result.stream.backgroundColor).toEqual(['#abcdef']);
    expect(result.stream.pointStyle).toEqual(['circle']);
    expect(result.stream.pointRadius).toEqual([4]);
  });

  it('drops a streaming point with no ttft_ms rather than plotting a phantom value', () => {
    const p = point({ is_streaming: true, ttft_ms: null, timed_out: false });
    const result = splitLatencyPoints([p], '#000');
    expect(result.stream.data).toEqual([]);
    expect(result.nonStream.data).toEqual([]);
  });

  it('drops a non-streaming point with no total_ms rather than plotting a phantom value', () => {
    const p = point({ is_streaming: false, total_ms: null, timed_out: false });
    const result = splitLatencyPoints([p], '#000');
    expect(result.nonStream.data).toEqual([]);
    expect(result.stream.data).toEqual([]);
  });

  it('returns empty datasets for an empty input', () => {
    const result = splitLatencyPoints([], '#000');
    expect(result.stream.data).toEqual([]);
    expect(result.nonStream.data).toEqual([]);
  });
});
