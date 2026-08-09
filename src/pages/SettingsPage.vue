<template>
  <q-page padding>
    <div class="text-h5 q-mb-md">Settings</div>

    <q-card flat bordered style="max-width: 640px" class="q-mb-md">
      <q-card-section>
        <div class="text-subtitle1 q-mb-md">General</div>
        <q-select
          v-model="form.timezone"
          :options="timezoneOptions"
          label="Timezone"
          outlined
          dense
          emit-value
          map-options
          class="q-mb-md"
        />
        <q-input
          v-model="form.public_base_url"
          label="Public Base URL"
          outlined
          dense
          clearable
          placeholder="https://example.com"
          hint="When set, this URL is shown as the Base URL on the public usage page (overrides the dashboard origin)"
          class="q-mb-md"
        />
        <q-input
          v-model="form.api_key_prefix"
          label="API Key Prefix"
          outlined
          dense
          clearable
          placeholder="sk-upstream:"
          hint="Prepended to the API key shown on the public usage page (display only — does not change auth)"
          class="q-mb-md"
        />
        <q-input
          v-model="form.openai_compat_base_url"
          label="OpenAI-compatible Base URL"
          outlined
          dense
          clearable
          placeholder="https://example.com/v1"
          hint="When set, this URL is shown on the public usage page with a copy button"
          class="q-mb-md"
        />
        <div v-if="saveError" class="text-negative text-caption q-mb-sm">{{ saveError }}</div>
        <q-btn color="primary" label="Save Settings" :loading="saving" @click="saveSettings" />
      </q-card-section>
    </q-card>

    <q-card flat bordered style="max-width: 640px">
      <q-card-section>
        <div class="text-subtitle1 q-mb-md">Telegram Alerts</div>

        <q-input
          v-model="form.telegram_bot_token"
          label="Bot Token"
          outlined
          dense
          clearable
          class="q-mb-sm"
        />

        <div class="q-mb-xs text-caption text-grey-7">Chat IDs</div>
        <div class="q-mb-xs">
          <q-chip
            v-for="id in form.telegram_chat_ids"
            :key="id"
            removable
            dense
            @remove="removeChatId(id)"
          >
            {{ id }}
          </q-chip>
          <span v-if="form.telegram_chat_ids.length === 0" class="text-grey text-caption">
            No chat IDs configured
          </span>
        </div>

        <q-btn
          outline
          dense
          size="sm"
          label="Get Chat IDs from bot"
          icon="search"
          class="q-mb-md"
          :loading="fetchingChats"
          @click="fetchChats"
        />

        <div v-if="chatsError" class="text-negative text-caption q-mb-sm">{{ chatsError }}</div>

        <q-input
          v-model="alertStatusCodesStr"
          label="Alert Status Codes (comma-separated)"
          outlined
          dense
          class="q-mb-sm"
          hint="e.g. 500,502,503"
        />

        <q-input
          v-model.number="form.alert_cooldown_mins"
          label="Cooldown (minutes)"
          outlined
          dense
          type="number"
          class="q-mb-md"
        />

        <div v-if="saveError" class="text-negative text-caption q-mb-sm">{{ saveError }}</div>

        <div class="row q-gutter-sm">
          <q-btn color="primary" label="Save Settings" :loading="saving" @click="saveSettings" />
          <q-btn outline label="Test Alert" :loading="testing" @click="testAlert" />
        </div>
      </q-card-section>
    </q-card>

    <q-card flat bordered style="max-width: 640px" class="q-mt-md">
      <q-card-section>
        <div class="text-subtitle1 q-mb-md">Blocked API Paths</div>

        <div class="q-mb-xs" role="group" aria-label="Blocked API Paths">
          <q-chip
            v-for="path in form.blocked_paths"
            :key="path"
            removable
            dense
            :remove-aria-label="`Remove ${path}`"
            @remove="removeBlockedPath(path)"
          >
            {{ path }}
          </q-chip>
          <span v-if="form.blocked_paths.length === 0" class="text-grey text-caption">
            No blocked paths
          </span>
        </div>

        <div class="row q-gutter-sm items-center q-mb-md">
          <q-input
            v-model="newBlockedPath"
            label="Add path (e.g. /v1/completions)"
            outlined
            dense
            class="col"
            @keyup.enter="addBlockedPath"
          />
          <q-btn outline dense label="Add" @click="addBlockedPath" />
        </div>

        <div v-if="saveError" class="text-negative text-caption q-mb-sm">{{ saveError }}</div>
        <q-btn color="primary" label="Save Settings" :loading="saving" @click="saveSettings" />
      </q-card-section>
    </q-card>

    <q-card flat bordered style="max-width: 640px" class="q-mt-md">
      <q-card-section>
        <div class="text-subtitle1 q-mb-md">User Custom Endpoints</div>

        <q-toggle
          v-model="form.user_endpoints_enabled"
          label="Enable user custom endpoints"
          class="q-mb-sm"
        />
        <div class="text-caption text-grey-7 q-mb-md">
          When disabled, custom endpoint routing is bypassed and the section is hidden from users.
        </div>

        <div v-if="saveError" class="text-negative text-caption q-mb-sm">{{ saveError }}</div>
        <q-btn color="primary" label="Save Settings" :loading="saving" @click="saveSettings" />
      </q-card-section>
    </q-card>

    <q-card flat bordered style="max-width: 640px" class="q-mt-md">
      <q-card-section>
        <div class="text-subtitle1 q-mb-md">Token Counting</div>

        <q-toggle
          v-model="form.ct_always_estimate"
          label="Always estimate locally"
          class="q-mb-sm"
        />
        <div class="text-caption text-grey-7 q-mb-md">
          Skip API calls and estimate tokens by character count (chars / 4)
        </div>

        <template v-if="!form.ct_always_estimate">
          <q-input
            v-model="form.ct_anthropic_base_url"
            label="Anthropic Base URL"
            outlined
            dense
            clearable
            placeholder="https://api.anthropic.com"
            class="q-mb-sm"
          />
          <q-input
            v-model="form.ct_anthropic_api_key"
            label="Anthropic API Key"
            outlined
            dense
            clearable
            class="q-mb-md"
          />
        </template>

        <q-separator class="q-my-md" />
        <div class="text-subtitle2 q-mb-sm">Non-streaming Timeout</div>
        <q-input
          v-model.number="form.default_non_stream_timeout_ms"
          label="Default timeout (ms, empty = unbounded)"
          outlined
          dense
          type="number"
          min="1"
          clearable
          style="max-width: 320px"
          class="q-mb-xs"
          @clear="form.default_non_stream_timeout_ms = null"
        />
        <div class="text-caption text-grey-7 q-mb-md">
          Applies to non-streaming requests on any upstream that has no timeout of its own
          (group server, bonus server, or user endpoint). When it fires, the proxy moves to
          the next entry in the waterfall instead of returning an error. Default 600000
          (10 min), matching Anthropic's own non-streaming ceiling. Clearing this restores
          the previous unbounded behaviour, where a hung upstream can hold a request for
          hours.
        </div>

        <div v-if="saveError" class="text-negative text-caption q-mb-sm">{{ saveError }}</div>
        <q-btn color="primary" label="Save Settings" :loading="saving" @click="saveSettings" />
      </q-card-section>
    </q-card>

    <q-card flat bordered style="max-width: 640px" class="q-mt-md">
      <q-card-section>
        <div class="text-subtitle1 q-mb-md">Database Maintenance</div>

        <q-input
          v-model.number="form.proxy_log_retention_days"
          label="Auto-delete logs older than (days)"
          outlined
          dense
          type="number"
          min="1"
          style="max-width: 320px"
          class="q-mb-xs"
          hint="A job runs daily at 00:00 (in the configured timezone) and removes logs older than this. Default 3."
        />

        <q-toggle
          v-model="form.log_request_body"
          label="Store request body in error logs"
          class="q-mt-md q-mb-xs"
        />
        <div class="text-caption text-grey-7 q-mb-md">
          When off, the request body is not stored (an empty <code>{}</code> is saved instead) to
          save disk space. The cURL command on the Proxy Error Logs page will have an empty body.
          Headers are unaffected. Default off.
        </div>
        <div class="q-mb-md">
          <q-btn
            color="primary"
            label="Save Settings"
            :loading="saving"
            @click="saveSettings"
          />
        </div>

        <q-separator class="q-mb-md" />

        <div class="text-caption text-grey-7 q-mb-sm">Manual purge</div>
        <div class="row q-gutter-sm items-center q-mb-sm">
          <q-select
            v-model="purgeKeepDays"
            :options="purgeKeepDaysOptions"
            label="Keep logs from last"
            outlined
            dense
            emit-value
            map-options
            style="min-width: 200px"
            @update:model-value="fetchPurgePreview"
          />
          <q-spinner v-if="fetchingPurgePreview" size="sm" />
        </div>

        <div
          v-if="purgePreviewCount > 0"
          class="text-warning q-mb-md"
        >
          &#9888; {{ purgePreviewCount }} logs older than {{ purgeKeepDays }} days will be permanently deleted
        </div>

        <q-btn
          color="negative"
          label="Purge Old Logs"
          icon="delete"
          :loading="purging"
          :disable="purgePreviewCount === 0 || fetchingPurgePreview"
          @click="showPurgeDialog = true"
        />
      </q-card-section>
    </q-card>

    <!-- Chat discovery dialog -->
    <q-dialog v-model="showChatsDialog">
      <q-card style="min-width: 400px">
        <q-card-section>
          <div class="text-h6">Select Chats to Add</div>
        </q-card-section>
        <q-card-section>
          <div v-if="discoveredChats.length === 0" class="text-grey">
            No chats found. Send a message to your bot first.
          </div>
          <q-list v-else dense>
            <q-item
              v-for="chat in discoveredChats"
              :key="chat.chat_id"
              tag="label"
              clickable
            >
              <q-item-section avatar>
                <q-checkbox v-model="selectedChats" :val="chat.chat_id" dense />
              </q-item-section>
              <q-item-section>
                <q-item-label>{{ chat.first_name || chat.username || chat.chat_id }}</q-item-label>
                <q-item-label caption>
                  {{ [chat.username ? `@${chat.username}` : null, `ID: ${chat.chat_id}`].filter(Boolean).join(' · ') }}
                </q-item-label>
              </q-item-section>
            </q-item>
          </q-list>
        </q-card-section>
        <q-card-actions align="right">
          <q-btn flat label="Cancel" v-close-popup />
          <q-btn
            v-if="discoveredChats.length > 0"
            color="primary"
            label="Add Selected"
            :disable="selectedChats.length === 0"
            @click="addSelectedChats"
            v-close-popup
          />
        </q-card-actions>
      </q-card>
    </q-dialog>

    <!-- Purge confirmation dialog -->
    <q-dialog v-model="showPurgeDialog">
      <q-card style="min-width: 400px">
        <q-card-section>
          <div class="text-h6">Confirm Purge</div>
        </q-card-section>
        <q-card-section>
          Are you sure you want to permanently delete {{ purgePreviewCount }} logs older than
          {{ purgeKeepDays }} days? This cannot be undone.
          <div class="text-warning q-mt-sm">
            &#9888; The current month's log table will be briefly locked while disk space is
            reclaimed (VACUUM FULL). New proxy logs may be delayed or dropped during this time;
            request relay is not affected.
          </div>
        </q-card-section>
        <q-card-actions align="right">
          <q-btn flat label="Cancel" v-close-popup />
          <q-btn color="negative" label="Delete" @click="confirmPurge" v-close-popup />
        </q-card-actions>
      </q-card>
    </q-dialog>
  </q-page>
</template>

<script setup lang="ts">
import { ref, onMounted, computed } from 'vue';
import { useQuasar } from 'quasar';
import { api } from 'boot/axios';

interface Settings {
  telegram_bot_token: string | null;
  telegram_chat_ids: string[];
  alert_status_codes: number[];
  alert_cooldown_mins: number;
  blocked_paths: string[];
  timezone: string;
  ct_always_estimate: boolean;
  ct_anthropic_base_url: string | null;
  ct_anthropic_api_key: string | null;
  user_endpoints_enabled: boolean;
  openai_compat_base_url: string | null;
  public_base_url: string | null;
  api_key_prefix: string | null;
  proxy_log_retention_days: number;
  log_request_body: boolean;
  /** Fallback non-streaming timeout; null means unbounded. */
  default_non_stream_timeout_ms: number | null;
}

interface TelegramChat {
  chat_id: string;
  first_name: string | null;
  username: string | null;
}

const $q = useQuasar();

const form = ref<Settings>({
  telegram_bot_token: null,
  telegram_chat_ids: [],
  alert_status_codes: [500, 502, 503],
  alert_cooldown_mins: 5,
  blocked_paths: [],
  timezone: 'Asia/Ho_Chi_Minh',
  ct_always_estimate: false,
  ct_anthropic_base_url: null,
  ct_anthropic_api_key: null,
  user_endpoints_enabled: true,
  openai_compat_base_url: null,
  public_base_url: null,
  api_key_prefix: null,
  proxy_log_retention_days: 3,
  log_request_body: false,
  default_non_stream_timeout_ms: 600000,
});

const alertStatusCodesStr = computed({
  get: () => form.value.alert_status_codes.join(','),
  set: (val: string) => {
    form.value.alert_status_codes = val
      .split(',')
      .map((s) => parseInt(s.trim(), 10))
      .filter((n) => !Number.isNaN(n));
  },
});

const saving = ref(false);
const testing = ref(false);
const fetchingChats = ref(false);
const saveError = ref('');
const chatsError = ref('');
const showChatsDialog = ref(false);
const discoveredChats = ref<TelegramChat[]>([]);
const selectedChats = ref<string[]>([]);
const newBlockedPath = ref('');

const timezoneOptions = [
  'UTC',
  'Asia/Ho_Chi_Minh',
  'Asia/Bangkok',
  'Asia/Singapore',
  'Asia/Tokyo',
  'Asia/Seoul',
  'Asia/Shanghai',
  'Asia/Hong_Kong',
  'Asia/Kolkata',
  'Australia/Sydney',
  'Europe/London',
  'Europe/Paris',
  'America/New_York',
  'America/Los_Angeles',
].map((tz) => ({ label: tz, value: tz }));

const purgeKeepDaysOptions = [
  { label: '1 day', value: 1 },
  { label: '3 days', value: 3 },
  { label: '7 days', value: 7 },
  { label: '14 days', value: 14 },
  { label: '30 days', value: 30 },
];
const purgeKeepDays = ref(7);
const purgePreviewCount = ref(0);
const fetchingPurgePreview = ref(false);
const purging = ref(false);
const showPurgeDialog = ref(false);

onMounted(async () => {
  try {
    const { data } = await api.get<Settings>('/api/admin/settings');
    form.value = { ...form.value, ...data, timezone: data.timezone || 'Asia/Ho_Chi_Minh' };
  } catch {
    // use defaults
  }
  fetchPurgePreview();
});

function removeChatId(id: string) {
  form.value.telegram_chat_ids = form.value.telegram_chat_ids.filter((c) => c !== id);
}

async function saveSettings() {
  saving.value = true;
  saveError.value = '';
  try {
    const { data } = await api.put<Settings>('/api/admin/settings', {
      telegram_bot_token: form.value.telegram_bot_token || null,
      telegram_chat_ids: form.value.telegram_chat_ids,
      alert_status_codes: form.value.alert_status_codes,
      alert_cooldown_mins: form.value.alert_cooldown_mins,
      blocked_paths: form.value.blocked_paths,
      timezone: form.value.timezone,
      ct_always_estimate: form.value.ct_always_estimate,
      ct_anthropic_base_url: form.value.ct_anthropic_base_url || null,
      ct_anthropic_api_key: form.value.ct_anthropic_api_key || null,
      user_endpoints_enabled: form.value.user_endpoints_enabled,
      openai_compat_base_url: form.value.openai_compat_base_url || null,
      public_base_url: form.value.public_base_url || null,
      api_key_prefix: form.value.api_key_prefix || null,
      proxy_log_retention_days: form.value.proxy_log_retention_days,
      log_request_body: form.value.log_request_body,
      // Sent even when null: an explicit null clears the default (back to unbounded),
      // which the backend distinguishes from the field being absent.
      default_non_stream_timeout_ms: form.value.default_non_stream_timeout_ms ?? null,
    });
    form.value = { ...form.value, ...data, timezone: data.timezone || 'Asia/Ho_Chi_Minh' };
    $q.notify({ type: 'positive', message: 'Settings saved' });
  } catch (err: unknown) {
    const msg = (err as { response?: { data?: { error?: string } } })?.response?.data?.error ?? 'Failed to save settings';
    saveError.value = msg;
  } finally {
    saving.value = false;
  }
}

async function testAlert() {
  testing.value = true;
  try {
    await api.post('/api/admin/settings/test');
    $q.notify({ type: 'positive', message: 'Test alert sent successfully' });
  } catch (err: unknown) {
    const msg = (err as { response?: { data?: { error?: string } } })?.response?.data?.error ?? 'Test alert failed';
    $q.notify({ type: 'negative', message: msg });
  } finally {
    testing.value = false;
  }
}

async function fetchChats() {
  fetchingChats.value = true;
  chatsError.value = '';
  try {
    const { data } = await api.get<{ chats: TelegramChat[] }>('/api/admin/settings/telegram-chats');
    discoveredChats.value = data.chats;
    selectedChats.value = [];
    showChatsDialog.value = true;
  } catch (err: unknown) {
    const msg = (err as { response?: { data?: { error?: string } } })?.response?.data?.error ?? 'Failed to fetch chats';
    chatsError.value = msg;
  } finally {
    fetchingChats.value = false;
  }
}

function addSelectedChats() {
  const existing = new Set(form.value.telegram_chat_ids);
  for (const id of selectedChats.value) {
    existing.add(id);
  }
  form.value.telegram_chat_ids = Array.from(existing);
}

function addBlockedPath() {
  const path = newBlockedPath.value.trim();
  if (!path) return;
  if (form.value.blocked_paths.includes(path)) {
    $q.notify({ type: 'warning', message: 'Path already in the list' });
    newBlockedPath.value = '';
    return;
  }
  form.value.blocked_paths.push(path);
  newBlockedPath.value = '';
}

function removeBlockedPath(path: string) {
  form.value.blocked_paths = form.value.blocked_paths.filter((p) => p !== path);
}

async function fetchPurgePreview() {
  fetchingPurgePreview.value = true;
  try {
    const { data } = await api.get<{ count: number; cutoff: string }>(
      `/api/admin/logs/purge-preview?keep_days=${purgeKeepDays.value}`,
    );
    purgePreviewCount.value = data.count;
  } catch {
    purgePreviewCount.value = 0;
  } finally {
    fetchingPurgePreview.value = false;
  }
}

async function confirmPurge() {
  purging.value = true;
  try {
    const { data } = await api.post<{ deleted: number }>('/api/admin/logs/purge', {
      keep_days: purgeKeepDays.value,
    });
    $q.notify({ type: 'positive', message: `Deleted ${data.deleted} logs` });
    await fetchPurgePreview();
  } catch (err: unknown) {
    const msg =
      (err as { response?: { data?: { error?: string } } })?.response?.data?.error ??
      'Purge failed';
    $q.notify({ type: 'negative', message: msg });
  } finally {
    purging.value = false;
  }
}
</script>
