import { defineStore } from 'pinia';
import { ref } from 'vue';
import { api } from 'boot/axios';

export interface Group {
  id: string;
  name: string;
  api_key: string;
  failover_status_codes: number[];
  is_active: boolean;
  ttft_timeout_ms: number | null;
  count_tokens_server_id: string | null;
  count_tokens_model_mappings: Record<string, string>;
  created_at: string;
  updated_at: string;
}

export interface Model {
  id: string;
  name: string;
  input_1m_usd: number | null;
  output_1m_usd: number | null;
  cache_write_1m_usd: number | null;
  cache_read_1m_usd: number | null;
  created_at: string;
}

export interface GroupServerDetail {
  server_id: string;
  short_id: number;
  server_name: string;
  base_url: string | null;
  api_key: string | null;
  priority: number;
  model_mappings: Record<string, string>;
  is_enabled: boolean;
  cb_max_failures: number | null;
  cb_window_seconds: number | null;
  cb_cooldown_seconds: number | null;
  rate_input: number | null;
  rate_output: number | null;
  rate_cache_write: number | null;
  rate_cache_read: number | null;
  max_requests: number | null;
  rate_window_seconds: number | null;
  per_key_max_requests: number | null;
  per_key_rate_window_seconds: number | null;
  normalize_cache_read: boolean;
  max_input_tokens: number | null;
  min_input_tokens: number | null;
  supported_models: string[];
  password_hash: string | null;
  active_hours_start: string | null;
  active_hours_end: string | null;
  active_hours_timezone: string | null;
  retry_status_codes: number[] | null;
  retry_count: number | null;
  retry_delay_seconds: number | null;
  custom_headers: Record<string, string> | null;
}

export interface GroupWithServers extends Group {
  servers: GroupServerDetail[];
  allowed_models: Model[];
}

interface PaginatedResponse {
  data: Group[];
  total: number;
  page: number;
  total_pages: number;
}

export interface CircuitStatus {
  server_id: string;
  model: string;
  remaining_seconds: number;
  state: 'open' | 'half_open';
}

export interface TtftDataPoint {
  created_at: string;
  ttft_ms: number | null;
  timed_out: boolean;
}

export interface ServerTtftStats {
  server_id: string;
  server_name: string;
  avg_ttft_ms: number | null;
  p50_ttft_ms: number | null;
  p95_ttft_ms: number | null;
  timeout_count: number;
  total_count: number;
  data_points: TtftDataPoint[];
}

export interface TtftStatsResponse {
  servers: ServerTtftStats[];
}

export interface ServerTokenUsage {
  server_id: string;
  server_name: string;
  model: string | null;
  total_input_tokens: number;
  total_output_tokens: number;
  total_cache_creation_tokens: number;
  total_cache_read_tokens: number;
  request_count: number;
  cost_usd: number | null;
}

export interface TokenUsageStats {
  servers: ServerTokenUsage[];
}

export interface KeyTokenUsage {
  group_key_id: string | null;
  key_name: string | null;
  api_key: string | null;
  created_at: string | null;
  total_input_tokens: number;
  total_output_tokens: number;
  total_cache_creation_tokens: number;
  total_cache_read_tokens: number;
  request_count: number;
  cost_usd: number | null;
  peak_tpm: number;
}

export interface KeyUsageResponse {
  keys: KeyTokenUsage[];
}

export interface SpamResult {
  group_key_id: string;
  api_key: string;
  key_name: string;
  spam_type: 'low_token' | 'duplicate_request';
  request_count: number;
  peak_rpm: number;
  detected_at: string;
}

export interface GroupKey {
  id: string;
  group_id: string;
  api_key: string;
  name: string;
  is_active: boolean;
  created_at: string;
  updated_at: string;
}

interface PaginatedGroupKeys {
  data: GroupKey[];
  total: number;
  page: number;
  total_pages: number;
}

export const useGroupsStore = defineStore('groups', () => {
  const groups = ref<Group[]>([]);
  const total = ref(0);
  const totalPages = ref(0);
  const loading = ref(false);

  async function fetchGroups(params?: {
    page?: number; limit?: number; search?: string;
    is_active?: boolean; server_id?: string;
  }) {
    loading.value = true;
    try {
      const { data } = await api.get<PaginatedResponse>('/api/admin/groups', { params });
      groups.value = data.data;
      total.value = data.total;
      totalPages.value = data.total_pages;
      return data;
    } finally {
      loading.value = false;
    }
  }

  async function getGroup(id: string) {
    const { data } = await api.get<GroupWithServers>(`/api/admin/groups/${id}`);
    return data;
  }

  async function createGroup(input: { name: string; failover_status_codes?: number[] }) {
    const { data } = await api.post<Group>('/api/admin/groups', input);
    return data;
  }

  async function updateGroup(id: string, input: { name?: string; failover_status_codes?: number[]; is_active?: boolean; ttft_timeout_ms?: number | null; count_tokens_server_id?: string | null; count_tokens_model_mappings?: Record<string, string> }) {
    const { data } = await api.put<Group>(`/api/admin/groups/${id}`, input);
    return data;
  }

  async function deleteGroup(id: string) {
    await api.delete(`/api/admin/groups/${id}`);
  }

  async function regenerateKey(id: string) {
    const { data } = await api.post<Group>(`/api/admin/groups/${id}/regenerate-key`);
    return data;
  }

  async function bulkActivate(ids: string[]) {
    await api.post('/api/admin/groups/bulk/activate', { ids });
  }

  async function bulkDeactivate(ids: string[]) {
    await api.post('/api/admin/groups/bulk/deactivate', { ids });
  }

  async function bulkDelete(ids: string[]) {
    await api.post('/api/admin/groups/bulk/delete', { ids });
  }

  async function bulkAssignServer(group_ids: string[], server_id: string, priority: number, model_mappings?: Record<string, string>) {
    await api.post('/api/admin/groups/bulk/assign-server', { group_ids, server_id, priority, model_mappings });
  }

  // Group-server assignment actions
  async function assignServer(groupId: string, input: { server_id: string; priority: number; model_mappings?: Record<string, string> }) {
    await api.post(`/api/admin/groups/${groupId}/servers`, input);
  }

  async function updateAssignment(
    groupId: string,
    serverId: string,
    input: {
      priority?: number;
      model_mappings?: Record<string, string>;
      is_enabled?: boolean;
      cb_max_failures?: number | null;
      cb_window_seconds?: number | null;
      cb_cooldown_seconds?: number | null;
      rate_input?: number | null;
      rate_output?: number | null;
      rate_cache_write?: number | null;
      rate_cache_read?: number | null;
      max_requests?: number | null;
      rate_window_seconds?: number | null;
      per_key_max_requests?: number | null;
      per_key_rate_window_seconds?: number | null;
      normalize_cache_read?: boolean;
      max_input_tokens?: number | null;
      min_input_tokens?: number | null;
      supported_models?: string[];
      active_hours_start?: string | null;
      active_hours_end?: string | null;
      active_hours_timezone?: string | null;
      retry_status_codes?: number[] | null;
      retry_count?: number | null;
      retry_delay_seconds?: number | null;
    },
  ) {
    await api.put(`/api/admin/groups/${groupId}/servers/${serverId}`, input);
  }

  async function removeServer(groupId: string, serverId: string) {
    await api.delete(`/api/admin/groups/${groupId}/servers/${serverId}`);
  }

  async function reorderServers(groupId: string, serverIds: string[]) {
    await api.put(`/api/admin/groups/${groupId}/servers/reorder`, { server_ids: serverIds });
  }

  async function fetchTtftStats(groupId: string, range?: { start: string; end: string }, groupKeyId?: string) {
    const params: Record<string, string> = { group_id: groupId };
    if (range) {
      params.start = range.start;
      params.end = range.end;
    } else {
      params.period = '1h';
    }
    if (groupKeyId) {
      params.group_key_id = groupKeyId;
    }
    const { data } = await api.get<TtftStatsResponse>('/api/admin/ttft-stats', { params });
    return data;
  }

  async function fetchCircuitStatus(groupId: string) {
    const { data } = await api.get<CircuitStatus[]>(
      `/api/admin/groups/${groupId}/circuit-status`,
    );
    return data;
  }

  async function fetchTokenUsageStats(
    groupId: string,
    params?: { start?: string; end?: string; period?: string; is_dynamic_key?: boolean; key_hash?: string; group_key_id?: string },
  ) {
    const qp: Record<string, string> = { group_id: groupId };
    if (params?.start) qp.start = params.start;
    if (params?.end) qp.end = params.end;
    if (params?.period) qp.period = params.period;
    if (params?.is_dynamic_key !== undefined) qp.is_dynamic_key = String(params.is_dynamic_key);
    if (params?.key_hash) qp.key_hash = params.key_hash;
    if (params?.group_key_id) qp.group_key_id = params.group_key_id;
    const { data } = await api.get<TokenUsageStats>('/api/admin/token-usage', { params: qp });
    return data;
  }

  async function fetchTokenUsageByKey(
    groupId: string,
    params?: { start?: string; end?: string; period?: string },
  ) {
    const qp: Record<string, string> = { group_id: groupId };
    if (params?.start) qp.start = params.start;
    if (params?.end) qp.end = params.end;
    if (params?.period) qp.period = params.period;
    const { data } = await api.get<KeyUsageResponse>('/api/admin/token-usage/by-key', { params: qp });
    return data;
  }

  async function fetchGroupKeys(
    groupId: string,
    params?: { page?: number; limit?: number; search?: string },
  ) {
    const { data } = await api.get<PaginatedGroupKeys>(`/api/admin/groups/${groupId}/keys`, { params });
    return data;
  }

  async function createGroupKey(groupId: string, name: string) {
    const { data } = await api.post<GroupKey>(`/api/admin/groups/${groupId}/keys`, { name });
    return data;
  }

  async function updateGroupKey(groupId: string, keyId: string, input: { name?: string; is_active?: boolean }) {
    const { data } = await api.patch<GroupKey>(`/api/admin/groups/${groupId}/keys/${keyId}`, input);
    return data;
  }

  async function regenerateGroupKey(groupId: string, keyId: string) {
    const { data } = await api.post<GroupKey>(`/api/admin/groups/${groupId}/keys/${keyId}/regenerate`);
    return data;
  }

  async function deleteGroupKey(groupId: string, keyId: string) {
    await api.delete(`/api/admin/groups/${groupId}/keys/${keyId}`);
  }

  async function bulkCreateGroupKeys(
    groupId: string,
    input: { count: number; plan_id: string; name_prefix?: string; custom_expires_at?: string },
  ) {
    const { data } = await api.post<GroupKey[]>(`/api/admin/groups/${groupId}/keys/bulk`, input);
    return data;
  }

  async function fetchKeyUsage(
    groupId: string,
    groupKeyId: string,
    params?: { period?: string; start?: string; end?: string },
  ) {
    return fetchTokenUsageStats(groupId, { ...params, group_key_id: groupKeyId });
  }

  // Group allowed models
  async function fetchGroupAllowedModels(groupId: string) {
    const { data } = await api.get<Model[]>(`/api/admin/groups/${groupId}/allowed-models`);
    return data;
  }

  async function addGroupAllowedModel(groupId: string, input: { model_id?: string; name?: string }) {
    const { data } = await api.post<Model>(`/api/admin/groups/${groupId}/allowed-models`, input);
    return data;
  }

  async function removeGroupAllowedModel(groupId: string, modelId: string) {
    await api.delete(`/api/admin/groups/${groupId}/allowed-models/${modelId}`);
  }

  // Key allowed models
  async function fetchKeyAllowedModels(groupId: string, keyId: string) {
    const { data } = await api.get<Model[]>(`/api/admin/groups/${groupId}/keys/${keyId}/allowed-models`);
    return data;
  }

  async function addKeyAllowedModel(groupId: string, keyId: string, modelId: string) {
    const { data } = await api.post<Model>(`/api/admin/groups/${groupId}/keys/${keyId}/allowed-models`, { model_id: modelId });
    return data;
  }

  async function removeKeyAllowedModel(groupId: string, keyId: string, modelId: string) {
    await api.delete(`/api/admin/groups/${groupId}/keys/${keyId}/allowed-models/${modelId}`);
  }

  // Key server assignments
  async function fetchKeyServers(groupId: string, keyId: string) {
    const { data } = await api.get<GroupServerDetail[]>(`/api/admin/groups/${groupId}/keys/${keyId}/servers`);
    return data;
  }

  async function assignKeyServer(groupId: string, keyId: string, serverId: string) {
    const { data } = await api.post<GroupServerDetail[]>(`/api/admin/groups/${groupId}/keys/${keyId}/servers`, { server_ids: [serverId] });
    return data;
  }

  async function removeKeyServer(groupId: string, keyId: string, serverId: string) {
    await api.delete(`/api/admin/groups/${groupId}/keys/${keyId}/servers/${serverId}`);
  }

  async function fetchSpamDetection(groupId: string, params?: { page?: number; limit?: number }) {
    const { data } = await api.get<{
      data: SpamResult[];
      total: number;
      page: number;
      total_pages: number;
    }>('/api/admin/spam-detection', { params: { group_id: groupId, ...params } });
    return data;
  }

  // User agent management
  async function fetchGroupUserAgents(groupId: string) {
    const { data } = await api.get<{ user_agent: string; first_seen_at: string }[]>(
      `/api/admin/groups/${groupId}/user-agents`,
    );
    return data;
  }

  async function fetchGroupBlockedUserAgents(groupId: string) {
    const { data } = await api.get<{ user_agent: string; created_at: string }[]>(
      `/api/admin/groups/${groupId}/user-agents/blocked`,
    );
    return data;
  }

  async function addGroupBlockedUserAgent(groupId: string, userAgent: string) {
    await api.post(`/api/admin/groups/${groupId}/user-agents/blocked`, { user_agent: userAgent });
  }

  async function removeGroupBlockedUserAgent(groupId: string, userAgent: string) {
    await api.delete(`/api/admin/groups/${groupId}/user-agents/blocked`, {
      data: { user_agent: userAgent },
    });
  }

  return {
    groups, total, totalPages, loading,
    fetchGroups, getGroup, createGroup, updateGroup, deleteGroup, regenerateKey,
    bulkActivate, bulkDeactivate, bulkDelete, bulkAssignServer,
    assignServer, updateAssignment, removeServer, reorderServers,
    fetchTtftStats, fetchCircuitStatus, fetchTokenUsageStats, fetchTokenUsageByKey,
    fetchGroupKeys, createGroupKey, updateGroupKey, regenerateGroupKey, deleteGroupKey, bulkCreateGroupKeys, fetchKeyUsage,
    fetchGroupAllowedModels, addGroupAllowedModel, removeGroupAllowedModel,
    fetchKeyAllowedModels, addKeyAllowedModel, removeKeyAllowedModel,
    fetchKeyServers, assignKeyServer, removeKeyServer,
    fetchSpamDetection,
    fetchGroupUserAgents, fetchGroupBlockedUserAgents, addGroupBlockedUserAgent, removeGroupBlockedUserAgent,
  };
});
