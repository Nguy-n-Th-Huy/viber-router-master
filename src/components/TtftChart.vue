<template>
  <q-card flat bordered>
    <q-card-section>
      <div class="row items-center q-mb-sm">
        <q-btn flat dense round icon="chevron_left" :disable="blockOffset >= maxOffset" @click="blockOffset++" />
        <div class="text-subtitle1 q-mx-sm">TTFT — {{ blockLabel }}</div>
        <q-btn flat dense round icon="chevron_right" :disable="blockOffset <= 0" @click="blockOffset--" />
        <q-space />
        <slot name="filter" />
        <q-btn-toggle
          v-model="windowHours"
          flat dense no-caps
          toggle-color="primary"
          :options="[
            { label: '1h', value: 1 },
            { label: '4h', value: 4 },
            { label: '8h', value: 8 },
            { label: '12h', value: 12 },
            { label: '24h', value: 24 },
          ]"
        />
      </div>
      <div v-if="loading && !stats" class="flex flex-center q-pa-lg">
        <q-spinner size="md" />
      </div>
      <q-banner v-else-if="error" class="bg-negative text-white q-mb-sm" rounded>
        {{ error }}
      </q-banner>
      <div v-else-if="chartData" style="height: 300px">
        <Scatter :key="blockOffset" :data="chartData" :options="chartOptions" />
      </div>
      <div v-else class="text-grey text-center q-pa-lg flex flex-center" style="height: 300px">
        No TTFT data in this period
      </div>
<!-- PLACEHOLDER_STATS_TABLE -->
      <div v-if="stats && stats.servers.length > 0" class="q-mt-md">
        <q-markup-table flat bordered dense>
          <thead>
            <tr>
              <th class="text-left" rowspan="2">Server</th>
              <th class="text-center" colspan="4">Streaming (TTFT)</th>
              <th class="text-center" colspan="4">Non-streaming (total)</th>
              <th class="text-right" rowspan="2">Timeouts</th>
            </tr>
            <tr>
              <th class="text-right">Avg</th>
              <th class="text-right">P50</th>
              <th class="text-right">P95</th>
              <th class="text-right">Count</th>
              <th class="text-right">Avg</th>
              <th class="text-right">P50</th>
              <th class="text-right">P95</th>
              <th class="text-right">Count</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="s in stats.servers" :key="`${s.server_id}:${s.source}`">
              <td>{{ s.server_name }}</td>
              <td class="text-right">{{ fmtMs(s.avg_ttft_ms) }}</td>
              <td class="text-right">{{ fmtMs(s.p50_ttft_ms) }}</td>
              <td class="text-right">{{ fmtMs(s.p95_ttft_ms) }}</td>
              <td class="text-right">{{ s.stream_count }}</td>
              <td class="text-right">{{ fmtMs(s.avg_total_ms) }}</td>
              <td class="text-right">{{ fmtMs(s.p50_total_ms) }}</td>
              <td class="text-right">{{ fmtMs(s.p95_total_ms) }}</td>
              <td class="text-right">{{ s.non_stream_count }}</td>
              <td class="text-right">{{ s.timeout_count }}</td>
            </tr>
          </tbody>
        </q-markup-table>
      </div>
    </q-card-section>
  </q-card>
</template>

<script setup lang="ts">
import { ref, computed, watch, onMounted, onUnmounted } from 'vue';
import { useGroupsStore, type TtftStatsResponse } from 'stores/groups';
import { Scatter } from 'vue-chartjs';
import {
  Chart as ChartJS,
  CategoryScale,
  LinearScale,
  PointElement,
  LineElement,
  Title,
  Tooltip,
  Legend,
  TimeScale,
} from 'chart.js';
import 'chartjs-adapter-date-fns';
import type { TooltipItem } from 'chart.js';
import { splitLatencyPoints } from 'src/utils/latencyDatasets';

ChartJS.register(CategoryScale, LinearScale, PointElement, LineElement, Title, Tooltip, Legend, TimeScale);

const CHART_COLORS = ['#1976D2', '#26A69A', '#FF6F00', '#AB47BC', '#EF5350', '#66BB6A', '#42A5F5', '#FFA726'];

const props = defineProps<{ groupId: string; groupKeyId?: string | undefined }>();
const groupsStore = useGroupsStore();

const stats = ref<TtftStatsResponse | null>(null);
const loading = ref(false);
const error = ref('');
const blockOffset = ref(0);
const windowHours = ref(4);
let refreshTimer: ReturnType<typeof setInterval> | null = null;
// PLACEHOLDER_SCRIPT_CONTINUED

const blockRange = computed(() => {
  const now = new Date();
  const windowMs = windowHours.value * 60 * 60 * 1000;
  const end = new Date(now.getTime() - blockOffset.value * windowMs);
  const start = new Date(end.getTime() - windowMs);
  return { start, end };
});

const maxOffset = computed(() => Math.floor(7 * 24 / windowHours.value));

const blockLabel = computed(() => {
  const { start, end } = blockRange.value;
  const fmt = (d: Date) => d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  const dateFmt = (d: Date) => d.toLocaleDateString([], { month: 'short', day: 'numeric' });
  const startDate = dateFmt(start);
  const endDate = dateFmt(end);
  const today = dateFmt(new Date());
  const dateStr = startDate === today ? 'Today' : startDate !== endDate ? `${startDate} – ${endDate}` : startDate;
  return `${fmt(start)} – ${fmt(end)}, ${dateStr}`;
});

async function loadStats() {
  if (!props.groupId) return;
  loading.value = true;
  error.value = '';
  try {
    const { start, end } = blockRange.value;
    stats.value = await groupsStore.fetchTtftStats(
      props.groupId,
      { start: start.toISOString(), end: end.toISOString() },
      props.groupKeyId,
    );
  } catch {
    error.value = 'Failed to load TTFT data';
  } finally {
    loading.value = false;
  }
}
// PLACEHOLDER_CHART_DATA

function fmtMs(v: number | null): string {
  return v != null ? `${Math.round(v)}ms` : '—';
}

// Streaming (ttft_ms) and non-streaming (total_ms) points are plotted as separate
// datasets per server — the two measure different things and must never share a
// point style, or a glance at the chart would read a completion time as a TTFT.
// The bucketing itself lives in splitLatencyPoints (unit-tested).
const chartData = computed(() => {
  if (!stats.value || stats.value.servers.length === 0) return null;
  const datasets = stats.value.servers
    .flatMap((server, idx) => {
      const color = CHART_COLORS[idx % CHART_COLORS.length] as string;
      const split = splitLatencyPoints(server.data_points, color);
      return [
        {
          label: `${server.server_name} (stream)`,
          data: split.stream.data,
          backgroundColor: split.stream.backgroundColor,
          borderColor: 'transparent',
          pointRadius: split.stream.pointRadius,
          pointStyle: split.stream.pointStyle,
        },
        {
          label: `${server.server_name} (non-stream)`,
          data: split.nonStream.data,
          backgroundColor: color,
          borderColor: 'transparent',
          pointRadius: 4,
          pointStyle: 'triangle',
        },
      ];
    })
    .filter((d) => d.data.length > 0);
  if (datasets.length === 0) return null;
  return { datasets };
});

const chartOptions = computed(() => ({
  responsive: true,
  maintainAspectRatio: false,
  animation: false as const,
  scales: {
    x: {
      type: 'time' as const,
      time: { unit: 'minute' as const, displayFormats: { minute: 'HH:mm' }, tooltipFormat: 'HH:mm:ss' },
      min: blockRange.value.start.getTime(),
      max: blockRange.value.end.getTime(),
      title: { display: false },
      ticks: { maxTicksLimit: 10 },
    },
    y: { beginAtZero: true, title: { display: true, text: 'Latency (ms)' } },
  },
  plugins: {
    legend: { position: 'top' as const },
    tooltip: {
      callbacks: {
        label: function (this: unknown, ctx: TooltipItem<'scatter'>) {
          const label = ctx.dataset?.label || '';
          if (ctx.parsed?.y === 0) return `${label} (timeout)`;
          return `${label}: ${ctx.parsed?.y}ms`;
        },
      },
    },
  },
}));

function startRefresh() {
  stopRefresh();
  if (blockOffset.value === 0) {
    refreshTimer = setInterval(loadStats, 30_000);
  }
}

function stopRefresh() {
  if (refreshTimer) {
    clearInterval(refreshTimer);
    refreshTimer = null;
  }
}

watch(blockOffset, () => { loadStats(); startRefresh(); });
watch(windowHours, () => { blockOffset.value = 0; loadStats(); startRefresh(); });
watch(() => props.groupKeyId, () => { loadStats(); });

onMounted(() => { loadStats(); startRefresh(); });
onUnmounted(() => { stopRefresh(); });
</script>
