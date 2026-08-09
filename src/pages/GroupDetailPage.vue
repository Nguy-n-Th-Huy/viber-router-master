<template>
  <q-page padding>
    <div v-if="group" class="q-gutter-md">
      <div class="row items-center">
        <q-btn flat icon="arrow_back" @click="$router.push('/groups')" />
        <div class="text-h5 q-ml-sm">{{ group.name }}</div>
        <q-space />
        <q-toggle v-model="group.is_active" label="Active" @update:model-value="saveGroup" />
      </div>

      <q-tabs v-model="activeTab" dense align="left" class="text-primary" active-color="primary" indicator-color="primary">
        <q-tab name="properties" label="Properties" />
        <q-tab name="servers" label="Servers" />
        <q-tab name="keys" label="Keys" />
        <q-tab name="allowed-models" label="Allowed Models" />
        <q-tab name="ttft" label="TTFT" />
        <q-tab name="token-usage" label="Token Usage" />
        <q-tab name="spam" label="Spam" />
        <q-tab name="user-agents" label="User Agents" />
      </q-tabs>
      <q-separator />

      <q-tab-panels v-model="activeTab" animated>
        <!-- Properties Tab -->
        <q-tab-panel name="properties" class="q-pa-none q-gutter-md">
          <q-card flat bordered>
            <q-card-section>
              <div class="text-subtitle1 q-mb-sm">Properties</div>
              <q-input v-model="group.name" label="Name" outlined dense class="q-mb-sm" />
              <q-input v-model="failoverCodesStr" label="Failover Status Codes (comma-separated)" outlined dense class="q-mb-sm" />
              <q-input v-model="ttftTimeoutStr" label="TTFT Timeout (ms, empty = disabled)" outlined dense type="number" class="q-mb-sm" hint="Auto-switch to next server if first token takes longer" />
              <q-btn color="primary" label="Save" @click="saveGroup" />
            </q-card-section>
          </q-card>

          <q-card flat bordered>
            <q-card-section>
              <div class="text-subtitle1 q-mb-sm">Count Tokens Default Server</div>
              <q-select v-model="group.count_tokens_server_id" :options="allServers" label="Default server for /v1/messages/count_tokens" outlined dense emit-value map-options clearable class="q-mb-sm" @update:model-value="saveGroup" />
              <div class="text-subtitle2 q-mb-xs q-mt-md">Count Tokens Model Mappings</div>
              <div v-for="(entry, idx) in ctMappingEntries" :key="idx" class="row q-gutter-sm q-mb-sm">
                <q-input v-model="entry.from" label="From model" outlined dense style="flex:1" />
                <q-input v-model="entry.to" label="To model" outlined dense style="flex:1" />
                <q-btn flat dense icon="close" @click="ctMappingEntries.splice(idx, 1)" />
              </div>
              <div class="row q-gutter-sm">
                <q-btn flat dense icon="add" label="Add mapping" @click="ctMappingEntries.push({ from: '', to: '' })" />
                <q-btn color="primary" label="Save Mappings" dense @click="saveCtMappings" />
              </div>
            </q-card-section>
          </q-card>

          <q-card flat bordered>
            <q-card-section>
              <div class="text-subtitle1 q-mb-sm">API Key</div>
              <div class="row items-center q-gutter-sm">
                <code class="text-body1">{{ group.api_key }}</code>
                <q-btn flat dense icon="content_copy" @click="copyKey" />
                <q-btn flat dense color="warning" label="Regenerate" @click="onRegenerate" />
              </div>
            </q-card-section>
          </q-card>

          <q-card flat bordered>
            <q-card-section>
              <div class="row items-center q-mb-sm">
                <div class="text-subtitle1">Key Builder</div>
                <q-space />
                <q-toggle v-model="showAllKeyBuilderServers" label="Show all servers" dense />
              </div>
              <div v-for="entry in visibleKeyBuilderEntries" :key="entry.server_id" class="row items-center q-gutter-sm q-mb-sm">
                <span class="text-body2" style="min-width: 160px">{{ entry.server_name }} (#{{ entry.short_id }})</span>
                <q-input v-model="entry.key" :placeholder="entry.defaultKey || 'API Key'" outlined dense style="flex: 1" />
              </div>
              <div v-if="visibleKeyBuilderEntries.length === 0" class="text-grey q-mb-sm">No servers without predefined key</div>
              <div v-if="builtKey" class="q-mt-sm">
                <div class="text-caption q-mb-xs">Dynamic Key</div>
                <div class="row items-center q-gutter-sm">
                  <code class="text-body2" style="word-break: break-all">{{ builtKey }}</code>
                  <q-btn flat dense icon="content_copy" @click="copyText(builtKey)" />
                </div>
              </div>
            </q-card-section>
          </q-card>
        </q-tab-panel>


        <!-- Servers Tab -->
        <q-tab-panel name="servers" class="q-pa-none">
          <q-card flat bordered>
            <q-card-section>
              <div class="row items-center q-mb-sm">
                <div class="text-subtitle1">Servers (priority order)</div>
                <q-space />
                <q-btn flat dense icon="add" label="Add Server" @click="showAddServer = true" />
              </div>
              <q-list bordered separator>
                <q-item v-for="(s, idx) in servers" :key="s.server_id" :class="{ 'disabled-server': !s.is_enabled }">
                  <q-item-section avatar>
                    <div class="column items-center">
                      <q-btn flat dense icon="arrow_upward" :disable="idx === 0" @click="moveServer(idx, -1)" />
                      <span class="text-caption">{{ s.priority }}</span>
                      <q-btn flat dense icon="arrow_downward" :disable="idx === servers.length - 1" @click="moveServer(idx, 1)" />
                    </div>
                  </q-item-section>
                  <q-item-section>
                    <q-item-label :class="{ 'text-strike': !s.is_enabled }">
                      {{ s.server_name }}
                      <q-badge outline class="q-ml-sm">
                        #{{ s.short_id }}
                        <q-btn flat dense size="xs" icon="content_copy" class="q-ml-xs" @click.stop="copyShortId(s.short_id)" />
                      </q-badge>
                      <q-badge v-for="cs in getCircuitStatuses(s.server_id)" :key="`${cs.server_id}:${cs.model}`" :color="cs.state === 'half_open' ? 'warning' : 'negative'" class="q-ml-sm">
                        <template v-if="cs.state === 'half_open'">
                          Circuit Probing [{{ formatCircuitModel(cs.model) }}]
                        </template>
                        <template v-else>
                          Circuit Open [{{ formatCircuitModel(cs.model) }}] ({{ formatCircuitRemaining(cs.remaining_seconds) }})
                        </template>
                      </q-badge>
                      <q-badge v-if="s.max_requests != null && s.rate_window_seconds != null" outline color="purple" class="q-ml-sm" :aria-label="`Rate limit: ${s.max_requests} requests per ${s.rate_window_seconds} seconds`">
                        {{ s.max_requests }}/{{ s.rate_window_seconds }}s
                      </q-badge>
                      <q-badge v-if="s.per_key_max_requests != null && s.per_key_rate_window_seconds != null" outline color="deep-purple" class="q-ml-sm" :aria-label="`Per-key rate limit: ${s.per_key_max_requests} requests per key per ${s.per_key_rate_window_seconds} seconds`">
                        {{ s.per_key_max_requests }}/key/{{ formatDurationSecs(s.per_key_rate_window_seconds) }}
                      </q-badge>
                      <q-badge v-if="s.max_input_tokens != null" outline color="teal" class="q-ml-sm" :aria-label="`Max input tokens: ${s.max_input_tokens}`">
                        ≤{{ formatTokenThreshold(s.max_input_tokens) }} tokens
                      </q-badge>
                      <q-badge v-if="s.min_input_tokens != null" outline color="orange" class="q-ml-sm" :aria-label="`Min input tokens: ${s.min_input_tokens}`">
                        ≥{{ formatTokenThreshold(s.min_input_tokens) }} tokens
                      </q-badge>
                      <q-badge
                        v-if="s.active_hours_start != null && s.active_hours_end != null && s.active_hours_timezone != null"
                        outline
                        color="blue"
                        class="q-ml-sm"
                        :aria-label="`Active hours: ${s.active_hours_start}-${s.active_hours_end} (${s.active_hours_timezone})`"
                      >
                        {{ s.active_hours_start }}-{{ s.active_hours_end }} ({{ s.active_hours_timezone }})
                      </q-badge>
                      <q-badge
                        v-if="s.retry_status_codes != null && s.retry_count != null && s.retry_delay_seconds != null"
                        outline
                        color="deep-orange"
                        class="q-ml-sm"
                        :aria-label="`Retry: ${s.retry_count}x on [${s.retry_status_codes?.join(',')}] delay ${s.retry_delay_seconds}s`"
                      >
                        retry x{{ s.retry_count }}
                      </q-badge>
                      <q-badge
                        v-if="s.non_stream_timeout_ms != null"
                        outline
                        color="teal"
                        class="q-ml-sm"
                        :aria-label="`Non-streaming timeout: ${s.non_stream_timeout_ms}ms`"
                      >
                        ns {{ s.non_stream_timeout_ms }}ms
                      </q-badge>
                    </q-item-label>
                    <q-item-label caption>
                      <template v-if="serversStore.isProtected(s.server_id) && !serversStore.isUnlocked(s.server_id)">
                        🔒 {{ s.server_name }}
                      </template>
                      <template v-else>{{ s.base_url }}</template>
                    </q-item-label>
                    <div class="q-mt-xs">
                      <template v-if="s.supported_models?.length">
                        <q-chip
                          v-for="m in s.supported_models"
                          :key="m"
                          dense
                          size="sm"
                          color="primary"
                          text-color="white"
                          removable
                          :disable="removingSupportedModel === `${s.server_id}:${m}`"
                          :aria-label="`Remove supported model ${m} from ${s.server_name}`"
                          @remove="onRemoveSupportedModel(s, m)"
                        >
                          {{ m }}
                        </q-chip>
                      </template>
                      <q-chip v-else dense size="sm" outline color="grey">All models</q-chip>
                    </div>
                    <div v-if="uptimeData[s.server_id]?.length" class="q-mt-xs">
                      <UptimeBars :buckets="uptimeData[s.server_id] ?? []" />
                    </div>
                    <div v-else-if="uptimeLoading" class="q-mt-xs">
                      <q-skeleton type="rect" height="24px" />
                    </div>
                    <div v-else-if="uptimeError" class="q-mt-xs text-caption text-negative">
                      {{ uptimeError }}
                      <q-btn flat dense size="xs" label="Retry" class="q-ml-xs" @click.stop="loadUptime" />
                    </div>
                  </q-item-section>
                  <q-item-section side>
                    <div class="row q-gutter-xs items-center">
                      <q-badge
                        outline
                        :color="hasNonDefaultRate(s) ? 'orange' : 'grey'"
                        class="cursor-pointer q-pa-xs"
                        tabindex="0"
                        role="button"
                        :aria-label="`Edit cost rates for ${s.server_name}`"
                        @click.stop="openRateModal(s)"
                        @keydown.enter.stop="openRateModal(s)"
                        @keydown.space.stop="openRateModal(s)"
                      >
                        x{{ displayRate(s) }}
                      </q-badge>
                      <q-toggle v-model="s.is_enabled" dense :aria-label="`${s.server_name} enabled`" @update:model-value="toggleServerEnabled(s)" />
                      <q-btn
                        v-if="serversStore.isProtected(s.server_id) && !serversStore.isUnlocked(s.server_id)"
                        flat
                        dense
                        icon="lock_open"
                        :aria-label="`Unlock server ${s.server_name}`"
                        @click.stop="unlockDialog(s.server_id)"
                      />
                      <q-btn flat dense icon="edit" :aria-label="`Edit server ${s.server_name}`" @click="openEditServer(s)" />
                      <q-btn flat dense icon="tune" :aria-label="`Edit model mappings for ${s.server_name}`" @click="editMappings(s)" />
                      <q-btn flat dense icon="replay" :aria-label="`Edit retry config for ${s.server_name}`" @click="openRetryDialog(s)" />
                      <q-btn flat dense icon="delete" color="negative" :aria-label="`Remove server ${s.server_name}`" @click="onRemoveServer(s)" />
                    </div>
                  </q-item-section>
                </q-item>
                <q-item v-if="servers.length === 0">
                  <q-item-section class="text-grey">No servers assigned</q-item-section>
                </q-item>
              </q-list>
            </q-card-section>
          </q-card>
        </q-tab-panel>


        <!-- Keys Tab -->
        <q-tab-panel name="keys" class="q-pa-none">
          <q-card flat bordered>
            <q-card-section>
              <div class="row items-center q-mb-sm">
                <div class="text-subtitle1">Sub-Keys</div>
                <q-space />
                <q-input v-model="subKeySearch" placeholder="Search by name or key" outlined dense clearable style="max-width: 250px" class="q-mr-sm" @update:model-value="loadSubKeys" />
                <q-btn color="primary" dense label="Create Key" @click="showCreateKey = true" />
                <q-btn flat dense icon="playlist_add" label="Bulk Create" color="secondary" class="q-ml-xs" @click="openBulkCreate" />
              </div>
              <div v-if="subKeysLoading && !subKeys.length" class="flex flex-center q-pa-lg"><q-spinner size="md" /></div>
              <q-banner v-else-if="subKeyError" class="bg-negative text-white q-mb-sm" rounded>
                {{ subKeyError }}
                <template #action>
                  <q-btn flat label="Retry" @click="loadSubKeys" />
                </template>
              </q-banner>
              <q-table
                v-else
                flat bordered dense
                :rows="subKeys"
                :columns="subKeyColumns"
                row-key="id"
                v-model:pagination="subKeyPagination"
                @request="onSubKeyRequest"
              >
                <template #body="props">
                  <q-tr :props="props" @click="onExpandSubKey(props)" @keydown.enter="onExpandSubKey(props)" tabindex="0" role="button" :aria-expanded="props.expand" class="cursor-pointer">
                    <q-td key="name" :props="props">{{ props.row.name }}</q-td>
                    <q-td key="api_key" :props="props">
                      <code>{{ maskKey(props.row.api_key) }}</code>
                      <q-btn flat dense size="xs" icon="content_copy" aria-label="Copy key" @click.stop="copyText(props.row.api_key)" />
                    </q-td>
                    <q-td key="is_active" :props="props">
                      <q-toggle :model-value="props.row.is_active" dense :aria-label="`${props.row.name} active`" @update:model-value="toggleSubKey(props.row, $event)" @click.stop />
                    </q-td>
                    <q-td key="actions" :props="props">
                      <q-btn flat dense round size="sm" icon="open_in_new" aria-label="Open usage page" :href="usageLink(props.row.api_key)" target="_blank" @click.stop>
                        <q-tooltip>Usage page</q-tooltip>
                      </q-btn>
                      <q-btn flat dense round size="sm" icon="autorenew" aria-label="Regenerate key" @click.stop="onRegenerateSubKey(props.row)">
                        <q-tooltip>Regenerate</q-tooltip>
                      </q-btn>
                      <q-btn flat dense round size="sm" icon="delete" color="negative" aria-label="Delete key" @click.stop="onDeleteSubKey(props.row)">
                        <q-tooltip>Delete</q-tooltip>
                      </q-btn>
                    </q-td>
                  </q-tr>
                  <q-tr v-if="props.expand" :props="props">
                    <q-td colspan="4" class="q-pa-sm">
                      <div v-if="allowedModels.length > 0" class="q-mb-md">
                        <div class="text-subtitle2 q-mb-xs">Allowed Models</div>
                        <div class="row q-gutter-xs q-mb-sm items-center">
                          <q-chip
                            v-for="km in getKeyAllowedModels(props.row.id)"
                            :key="km.id"
                            removable
                            dense
                            color="primary"
                            text-color="white"
                            :disable="!!keyModelsLoading[props.row.id]"
                            @remove="onRemoveKeyModel(props.row.id, km.id)"
                          >
                            {{ km.name }}
                          </q-chip>
                          <q-spinner v-if="keyModelsLoading[props.row.id]" size="xs" class="q-ml-xs" />
                          <span v-else-if="getKeyAllowedModels(props.row.id).length === 0" class="text-caption text-grey">Inherits all group models</span>
                        </div>
                        <q-select
                          :model-value="null"
                          :options="keyModelOptions(props.row.id)"
                          label="Add model restriction"
                          outlined dense
                          emit-value map-options
                          :loading="!!keyModelsLoading[props.row.id]"
                          :disable="!!keyModelsLoading[props.row.id]"
                          style="max-width: 300px"
                          @update:model-value="(v: string) => onAddKeyModel(props.row.id, v)"
                        />
                      </div>
                      <div class="q-mb-md">
                        <div class="row items-center q-mb-xs">
                          <div class="text-subtitle2">Subscriptions</div>
                          <q-space />
                          <q-btn flat dense icon="bolt" label="Add Bonus" color="secondary" class="q-mr-xs" @click.stop="onOpenAddBonus(props.row.id)" />
                          <q-btn v-if="activePlans.length > 0" flat dense icon="add" label="Add Subscription" color="primary" @click="onOpenAssignSub(props.row.id)" />
                          <span v-else class="text-caption text-grey q-ml-sm">No active plans available</span>
                        </div>
                        <div v-if="keySubsLoading[props.row.id]" class="flex flex-center q-pa-sm q-mb-sm"><q-spinner size="sm" /></div>
                        <div v-else-if="!keySubscriptions[props.row.id] || keySubscriptions[props.row.id]?.total === 0" class="text-caption text-grey q-mb-sm">
                          No subscriptions &mdash; unlimited usage
                        </div>
                        <q-table
                          v-else
                          flat bordered dense
                          :rows="keySubscriptions[props.row.id]?.data ?? []"
                          :columns="subColumns"
                          row-key="id"
                          v-model:pagination="subPagination[props.row.id]"
                          @request="(p: { pagination: { page: number; rowsPerPage: number } }) => onSubPaginationRequest(props.row.id, p)"
                        >
                          <template #body-cell-status="sProps">
                            <q-td :props="sProps">
                              <q-badge :color="subStatusColor(sProps.row.status)" :label="sProps.row.status" />
                            </q-td>
                          </template>
                          <template #body-cell-cost_used="sProps">
                            <q-td :props="sProps">
                              <template v-if="sProps.row.sub_type === 'bonus'">N/A</template>
                              <template v-else>${{ sProps.row.cost_used.toFixed(2) }} / ${{ sProps.row.cost_limit_usd.toFixed(2) }}{{ sProps.row.sub_type === 'hourly_reset' ? ' (window)' : '' }}</template>
                            </q-td>
                          </template>
                          <template #body-cell-cost_limit_usd="sProps">
                            <q-td :props="sProps">
                              <template v-if="sProps.row.sub_type === 'bonus'">N/A</template>
                              <template v-else>${{ sProps.row.cost_limit_usd.toFixed(2) }}</template>
                            </q-td>
                          </template>
                          <template #body-cell-weekly_cost_limit_usd="sProps">
                            <q-td :props="sProps">
                              <template v-if="sProps.row.sub_type === 'bonus'">N/A</template>
                              <template v-else-if="sProps.row.weekly_cost_limit_usd != null">${{ sProps.row.weekly_cost_limit_usd.toFixed(2) }}</template>
                              <template v-else>Unlimited</template>
                            </q-td>
                          </template>
                          <template #body-cell-bonus_allowed_models="sProps">
                            <q-td :props="sProps">
                              <template v-if="sProps.row.sub_type === 'bonus'">
                                <button
                                  v-if="canEditBonusAllowedModels(sProps.row)"
                                  type="button"
                                  class="bonus-models-cell"
                                  @click.stop="onOpenEditBonusModels(props.row.id, sProps.row)"
                                >
                                  <div v-if="sProps.row.bonus_allowed_models?.length" class="row q-gutter-xs">
                                    <q-badge
                                      v-for="modelName in sProps.row.bonus_allowed_models"
                                      :key="modelName"
                                      color="secondary"
                                    >
                                      {{ modelName }}
                                    </q-badge>
                                  </div>
                                  <span v-else class="text-grey">All models</span>
                                  <q-icon name="edit" size="xs" class="q-ml-xs text-grey" />
                                </button>
                                <template v-else>
                                  <div v-if="sProps.row.bonus_allowed_models?.length" class="row q-gutter-xs">
                                    <q-badge
                                      v-for="modelName in sProps.row.bonus_allowed_models"
                                      :key="modelName"
                                      color="secondary"
                                    >
                                      {{ modelName }}
                                    </q-badge>
                                  </div>
                                  <span v-else class="text-grey">All models</span>
                                </template>
                              </template>
                              <span v-else>—</span>
                            </q-td>
                          </template>
                          <template #body-cell-actions="sProps">
                            <q-td :props="sProps">
                              <q-btn
                                v-if="sProps.row.sub_type === 'bonus'"
                                flat dense size="sm"
                                icon="arrow_upward"
                                :disable="getBonusIndex(props.row.id, sProps.row.id) <= 0"
                                :loading="reorderingBonusSubs.has(sProps.row.id)"
                                @click.stop="onMoveBonus(props.row.id, sProps.row.id, 'up')"
                              />
                              <q-btn
                                v-if="sProps.row.sub_type === 'bonus'"
                                flat dense size="sm"
                                icon="arrow_downward"
                                :disable="getBonusIndex(props.row.id, sProps.row.id) >= getBonusCount(props.row.id) - 1"
                                :loading="reorderingBonusSubs.has(sProps.row.id)"
                                @click.stop="onMoveBonus(props.row.id, sProps.row.id, 'down')"
                              />
                              <q-btn
                                v-if="sProps.row.sub_type === 'bonus'"
                                flat dense size="sm"
                                icon="vpn_key"
                                @click.stop="onOpenEditBonusHeaders(props.row.id, sProps.row)"
                              >
                                <q-tooltip>Edit headers</q-tooltip>
                              </q-btn>
                              <q-btn v-if="sProps.row.status === 'active'" flat dense size="sm" label="Cancel" color="negative" @click.stop="onCancelSubscription(props.row.id, sProps.row.id)" />
                            </q-td>
                          </template>
                        </q-table>
                      </div>
                      <div v-if="servers.length > 0" class="q-mb-md">
                        <div class="row items-center q-mb-xs">
                          <div class="text-subtitle2">Servers</div>
                          <q-space />
                          <q-btn-dropdown
                            v-if="keyServerOptions(props.row.id).length > 0"
                            flat dense icon="add" label="Add Server" color="primary"
                            :loading="!!keyServersLoading[props.row.id]"
                            :disable="!!keyServersLoading[props.row.id]"
                          >
                            <q-list dense style="min-width: 200px">
                              <q-item
                                v-for="gs in keyServerOptions(props.row.id)"
                                :key="gs.server_id"
                                clickable v-close-popup
                                @click="onAddKeyServer(props.row.id, gs.server_id)"
                              >
                                <q-item-section>
                                  <q-item-label>{{ gs.server_name }}</q-item-label>
                                  <q-item-label caption>{{ gs.base_url }}</q-item-label>
                                </q-item-section>
                              </q-item>
                            </q-list>
                          </q-btn-dropdown>
                        </div>
                        <div v-if="getKeyServers(props.row.id).length === 0" class="text-caption text-grey q-mb-sm">
                          Inherits all group servers
                        </div>
                        <q-list v-else dense bordered separator>
                          <q-item v-for="ks in getKeyServers(props.row.id)" :key="ks.server_id">
                            <q-item-section>
                              <q-item-label>{{ ks.server_name }}</q-item-label>
                              <q-item-label caption>{{ ks.base_url }}</q-item-label>
                            </q-item-section>
                            <q-item-section side>
                              <q-btn
                                flat dense icon="close" color="negative"
                                :loading="!!keyServersLoading[props.row.id]"
                                :disable="!!keyServersLoading[props.row.id]"
                                aria-label="Remove server from key"
                                @click="onRemoveKeyServer(props.row.id, ks.server_id)"
                              />
                            </q-item-section>
                          </q-item>
                        </q-list>
                      </div>
                      <SubKeyUsage :group-id="group?.id ?? ''" :group-key-id="props.row.id" />
                      <div class="q-mt-sm">
                        <TtftChart :group-id="group?.id ?? ''" :group-key-id="props.row.id" />
                      </div>
                    </q-td>
                  </q-tr>
                </template>
                <template #no-data>
                  <div class="text-grey text-center q-pa-md">No sub-keys created. Click "Create Key" to add one.</div>
                </template>
              </q-table>
            </q-card-section>
          </q-card>
        </q-tab-panel>

        <!-- Allowed Models Tab -->
        <q-tab-panel name="allowed-models" class="q-pa-none">
          <q-card flat bordered>
            <q-card-section>
              <div class="row items-center q-mb-sm">
                <div class="text-subtitle1">Allowed Models</div>
                <q-space />
                <div class="text-caption text-grey q-mr-md">{{ allowedModels.length === 0 ? 'No restrictions — all models allowed' : `${allowedModels.length} model(s) allowed` }}</div>
              </div>
              <div class="row q-gutter-sm q-mb-md">
                <q-select
                  v-model="modelToAdd"
                  :options="filteredModelOptions"
                  label="Add model"
                  outlined dense
                  use-input
                  input-debounce="200"
                  emit-value map-options
                  clearable
                  :loading="allowedModelsLoading"
                  style="min-width: 300px"
                  new-value-mode="add-unique"
                  @filter="onModelFilter"
                  @new-value="onNewModelValue"
                  @update:model-value="onAddAllowedModel"
                >
                  <template #no-option>
                    <q-item>
                      <q-item-section class="text-grey">
                        {{ modelSearchText ? `Press Enter to create "${modelSearchText}"` : 'Type to search or create' }}
                      </q-item-section>
                    </q-item>
                  </template>
                </q-select>
              </div>
              <q-list v-if="allowedModels.length > 0" bordered separator>
                <q-item v-for="m in allowedModels" :key="m.id">
                  <q-item-section>
                    <q-item-label>{{ m.name }}</q-item-label>
                  </q-item-section>
                  <q-item-section side>
                    <q-btn flat dense icon="close" color="negative" :loading="allowedModelsLoading" :aria-label="`Remove ${m.name}`" @click="onRemoveAllowedModel(m)" />
                  </q-item-section>
                </q-item>
              </q-list>
              <div v-else class="text-grey q-pa-md text-center">
                No model restrictions configured. Add models to restrict which models this group can use.
              </div>
            </q-card-section>
          </q-card>
        </q-tab-panel>

        <!-- TTFT Tab -->
        <q-tab-panel name="ttft" class="q-pa-none">
          <TtftChart :group-id="group?.id ?? ''" :group-key-id="ttftKeyFilter || undefined">
            <template #filter>
              <q-select
                v-model="ttftKeyFilter"
                :options="ttftKeyOptions"
                emit-value map-options
                outlined dense
                style="min-width: 180px"
                class="q-mr-sm"
              />
            </template>
          </TtftChart>
        </q-tab-panel>

        <!-- Token Usage Tab -->
        <q-tab-panel name="token-usage" class="q-pa-none">
          <q-tabs v-model="tokenUsageChildTab" dense inline-label align="left" class="text-grey" active-color="primary" indicator-color="primary">
            <q-tab name="by-server" label="By Server" />
            <q-tab name="by-sub-key" label="By Sub-Key" />
          </q-tabs>
          <q-separator />

          <q-tab-panels v-model="tokenUsageChildTab" animated>
            <!-- By Server child tab -->
            <q-tab-panel name="by-server" class="q-pa-none">
              <q-card flat bordered>
                <q-card-section>
          <div class="row items-center q-mb-sm">
            <div class="text-subtitle1">Token Usage</div>
            <q-space />
            <q-btn-toggle
              v-model="tokenUsagePeriod"
              flat dense no-caps
              class="period-toggle"
              toggle-color="primary"
              :options="[
                { label: 'Today', value: 'today' },
                { label: 'Yesterday', value: 'yesterday' },
                { label: 'Week', value: 'week' },
                { label: 'Month', value: 'month' },
              ]"
            />
          </div>
          <div class="row q-gutter-sm q-mb-sm">
            <q-select
              v-model="tokenUsageServerFilter"
              :options="tokenUsageServerOptions"
              label="Server"
              outlined dense
              emit-value map-options clearable
              style="min-width: 200px"
            />
            <q-toggle v-model="tokenUsageDynamicKeyFilter" label="Dynamic keys only" dense />
            <q-input v-model="tokenUsageKeyHashFilter" label="Key hash" outlined dense clearable style="max-width: 200px" />
            <q-btn flat dense icon="refresh" @click="loadTokenUsage" />
          </div>
          <div v-if="tokenUsageLoading && !tokenUsageStats" class="flex flex-center q-pa-lg">
            <q-spinner size="md" />
          </div>
          <q-banner v-else-if="tokenUsageError" class="bg-negative text-white q-mb-sm" rounded>
            {{ tokenUsageError }}
            <template #action>
              <q-btn flat label="Retry" @click="loadTokenUsage" />
            </template>
          </q-banner>
          <q-banner v-else-if="tokenUsageStats && tokenUsageStats.servers.length === 0" class="q-mb-sm" rounded>
            No token usage data in this period
          </q-banner>
          <q-table
            v-else-if="tokenUsageStats && tokenUsageStats.servers.length > 0"
            flat bordered dense
            :rows="filteredTokenUsageRows"
            :columns="tokenUsageColumns"
            row-key="__key"
            :pagination="{ rowsPerPage: 0 }"
            hide-pagination
          >
            <template #bottom-row>
              <q-tr class="text-weight-bold">
                <q-td>Total</q-td>
                <q-td />
                <q-td class="text-right">{{ totalTokenUsage.input.toLocaleString() }}</q-td>
                <q-td class="text-right">{{ totalTokenUsage.output.toLocaleString() }}</q-td>
                <q-td class="text-right">{{ totalTokenUsage.cacheCreation.toLocaleString() }}</q-td>
                <q-td class="text-right">{{ totalTokenUsage.cacheRead.toLocaleString() }}</q-td>
                <q-td class="text-right">{{ totalTokenUsage.requests.toLocaleString() }}</q-td>
                <q-td class="text-right">{{ totalTokenUsage.cost != null ? `$${totalTokenUsage.cost.toFixed(4)}` : '\u2014' }}</q-td>
              </q-tr>
            </template>
          </q-table>
                </q-card-section>
              </q-card>
            </q-tab-panel>

            <!-- By Sub-Key child tab -->
            <q-tab-panel name="by-sub-key" class="q-pa-none">
              <q-card flat bordered>
                <q-card-section>
                  <div class="row items-center q-mb-sm q-gutter-sm">
                    <div class="text-subtitle1">Usage by Sub-Key</div>
                    <q-space />
                    <q-input v-model="subKeyDateStart" type="date" label="Start" outlined dense style="max-width: 170px" />
                    <q-input v-model="subKeyDateEnd" type="date" label="End" outlined dense style="max-width: 170px" />
                    <q-btn flat dense icon="refresh" @click="loadSubKeyUsage" />
                  </div>
                  <div v-if="subKeyUsageLoading && subKeyUsageData.length === 0" class="flex flex-center q-pa-lg">
                    <q-spinner size="md" />
                  </div>
                  <q-banner v-else-if="subKeyUsageError" class="bg-negative text-white q-mb-sm" rounded>
                    {{ subKeyUsageError }}
                    <template #action>
                      <q-btn flat label="Retry" @click="loadSubKeyUsage" />
                    </template>
                  </q-banner>
                  <q-banner v-else-if="subKeyUsageData.length === 0 && !subKeyUsageLoading" class="q-mb-sm" rounded>
                    No usage data for this period
                  </q-banner>
                  <q-table
                    v-else-if="subKeyUsageData.length > 0"
                    flat bordered dense
                    :rows="subKeyUsageData"
                    :columns="subKeyUsageColumns"
                    :row-key="subKeyRowKey"
                    :pagination="{ rowsPerPage: 0 }"
                    hide-pagination
                    :loading="subKeyUsageLoading"
                  >
                    <template #body="props">
                      <q-tr
                        :props="props"
                        :class="props.row.group_key_id != null ? 'cursor-pointer' : ''"
                        @click="onExpandSubKeyUsageRow(props)"
                      >
                        <q-td key="index" :props="props" class="text-center">{{ props.rowIndex + 1 }}</q-td>
                        <q-td key="key_name" :props="props">
                          <q-icon
                            v-if="props.row.group_key_id != null"
                            :name="props.expand ? 'expand_less' : 'expand_more'"
                            size="xs"
                            class="q-mr-xs"
                          />
                          {{ props.row.key_name != null ? props.row.key_name : (props.row.group_key_id == null ? 'Master / Dynamic Keys' : 'Deleted Key') }}
                        </q-td>
                        <q-td key="input" :props="props" class="text-right">{{ formatCompact(props.row.total_input_tokens) }}</q-td>
                        <q-td key="output" :props="props" class="text-right">{{ formatCompact(props.row.total_output_tokens) }}</q-td>
                        <q-td key="cache_creation" :props="props" class="text-right">{{ formatCompact(props.row.total_cache_creation_tokens) }}</q-td>
                        <q-td key="cache_read" :props="props" class="text-right">{{ formatCompact(props.row.total_cache_read_tokens) }}</q-td>
                        <q-td key="requests" :props="props" class="text-right">{{ formatCompact(props.row.request_count) }}</q-td>
                        <q-td key="peak_tpm" :props="props" class="text-right">{{ formatCompact(props.row.peak_tpm ?? 0) }}</q-td>
                        <q-td key="cost" :props="props" class="text-right">{{ props.row.cost_usd != null ? `$${props.row.cost_usd.toFixed(4)}` : '\u2014' }}</q-td>
                        <q-td key="created_at" :props="props">{{ props.row.created_at != null ? props.row.created_at.slice(0, 10) : '\u2014' }}</q-td>
                        <q-td key="actions" :props="props">
                          <q-btn v-if="props.row.api_key" flat dense round size="sm" icon="open_in_new" aria-label="Open usage page" :href="usageLink(props.row.api_key)" target="_blank" @click.stop>
                            <q-tooltip>Usage page</q-tooltip>
                          </q-btn>
                        </q-td>
                      </q-tr>
                      <q-tr v-if="props.expand && props.row.group_key_id != null" :props="props">
                        <q-td colspan="11" class="q-pa-sm">
                          <div class="row items-center q-mb-xs">
                            <div class="text-subtitle2">Subscriptions</div>
                            <q-space />
                            <q-btn flat dense icon="bolt" label="Add Bonus" color="secondary" class="q-mr-xs" @click.stop="onOpenAddBonus(props.row.group_key_id ?? '')" />
                            <q-btn v-if="activePlans.length > 0" flat dense icon="add" label="Add Subscription" color="primary" @click="onOpenAssignSub(props.row.group_key_id ?? '')" />
                            <span v-else class="text-caption text-grey q-ml-sm">No active plans available</span>
                          </div>
                          <div v-if="keySubsLoading[props.row.group_key_id ?? '']" class="flex flex-center q-pa-sm q-mb-sm"><q-spinner size="sm" /></div>
                          <div v-else-if="!keySubscriptions[props.row.group_key_id ?? ''] || keySubscriptions[props.row.group_key_id ?? '']?.total === 0" class="text-caption text-grey q-mb-sm">
                            No subscriptions &mdash; unlimited usage
                          </div>
                          <q-table
                            v-else
                            flat bordered dense
                            :rows="keySubscriptions[props.row.group_key_id ?? '']?.data ?? []"
                            :columns="subColumns"
                            row-key="id"
                            v-model:pagination="subPagination[props.row.group_key_id ?? '']"
                            @request="(p: { pagination: { page: number; rowsPerPage: number } }) => onSubPaginationRequest(props.row.group_key_id ?? '', p)"
                          >
                            <template #body-cell-status="sProps">
                              <q-td :props="sProps">
                                <q-badge :color="subStatusColor(sProps.row.status)" :label="sProps.row.status" />
                              </q-td>
                            </template>
                            <template #body-cell-cost_used="sProps">
                              <q-td :props="sProps">
                                <template v-if="sProps.row.sub_type === 'bonus'">N/A</template>
                                <template v-else>${{ sProps.row.cost_used.toFixed(2) }} / ${{ sProps.row.cost_limit_usd.toFixed(2) }}{{ sProps.row.sub_type === 'hourly_reset' ? ' (window)' : '' }}</template>
                              </q-td>
                            </template>
                            <template #body-cell-cost_limit_usd="sProps">
                              <q-td :props="sProps">
                                <template v-if="sProps.row.sub_type === 'bonus'">N/A</template>
                                <template v-else>${{ sProps.row.cost_limit_usd.toFixed(2) }}</template>
                              </q-td>
                            </template>
                            <template #body-cell-weekly_cost_limit_usd="sProps">
                              <q-td :props="sProps">
                                <template v-if="sProps.row.sub_type === 'bonus'">N/A</template>
                                <template v-else-if="sProps.row.weekly_cost_limit_usd != null">${{ sProps.row.weekly_cost_limit_usd.toFixed(2) }}</template>
                                <template v-else>Unlimited</template>
                              </q-td>
                            </template>
                            <template #body-cell-bonus_allowed_models="sProps">
                              <q-td :props="sProps">
                                <template v-if="sProps.row.sub_type === 'bonus'">
                                  <button
                                    v-if="canEditBonusAllowedModels(sProps.row)"
                                    type="button"
                                    class="bonus-models-cell"
                                    @click.stop="onOpenEditBonusModels(props.row.group_key_id ?? '', sProps.row)"
                                  >
                                    <div v-if="sProps.row.bonus_allowed_models?.length" class="row q-gutter-xs">
                                      <q-chip
                                        v-for="modelName in sProps.row.bonus_allowed_models"
                                        :key="modelName"
                                        dense
                                        square
                                        color="secondary"
                                        text-color="white"
                                      >
                                        {{ modelName }}
                                      </q-chip>
                                    </div>
                                    <span v-else>All models</span>
                                    <q-icon name="edit" size="xs" class="q-ml-xs text-grey" />
                                  </button>
                                  <template v-else>
                                    <div v-if="sProps.row.bonus_allowed_models?.length" class="row q-gutter-xs">
                                      <q-chip
                                        v-for="modelName in sProps.row.bonus_allowed_models"
                                        :key="modelName"
                                        dense
                                        square
                                        color="secondary"
                                        text-color="white"
                                      >
                                        {{ modelName }}
                                      </q-chip>
                                    </div>
                                    <span v-else>All models</span>
                                  </template>
                                </template>
                                <span v-else>—</span>
                              </q-td>
                            </template>
                            <template #body-cell-actions="sProps">
                              <q-td :props="sProps">
                                <q-btn
                                  v-if="sProps.row.sub_type === 'bonus'"
                                  flat dense size="sm"
                                  icon="arrow_upward"
                                  :disable="getBonusIndex(props.row.group_key_id ?? '', sProps.row.id) <= 0"
                                  :loading="reorderingBonusSubs.has(sProps.row.id)"
                                  @click.stop="onMoveBonus(props.row.group_key_id ?? '', sProps.row.id, 'up')"
                                />
                                <q-btn
                                  v-if="sProps.row.sub_type === 'bonus'"
                                  flat dense size="sm"
                                  icon="arrow_downward"
                                  :disable="getBonusIndex(props.row.group_key_id ?? '', sProps.row.id) >= getBonusCount(props.row.group_key_id ?? '') - 1"
                                  :loading="reorderingBonusSubs.has(sProps.row.id)"
                                  @click.stop="onMoveBonus(props.row.group_key_id ?? '', sProps.row.id, 'down')"
                                />
                                <q-btn
                                  v-if="sProps.row.sub_type === 'bonus'"
                                  flat dense size="sm"
                                  icon="vpn_key"
                                  @click.stop="onOpenEditBonusHeaders(props.row.group_key_id ?? '', sProps.row)"
                                >
                                  <q-tooltip>Edit headers</q-tooltip>
                                </q-btn>
                                <q-btn v-if="sProps.row.status === 'active'" flat dense size="sm" label="Cancel" color="negative" @click.stop="onCancelSubscription(props.row.group_key_id ?? '', sProps.row.id)" />
                              </q-td>
                            </template>
                          </q-table>
                        </q-td>
                      </q-tr>
                    </template>
                    <template #bottom-row>
                      <q-tr class="text-weight-bold">
                        <q-td />
                        <q-td>Total</q-td>
                        <q-td class="text-right">{{ totalSubKeyUsage.input.toLocaleString() }}</q-td>
                        <q-td class="text-right">{{ totalSubKeyUsage.output.toLocaleString() }}</q-td>
                        <q-td class="text-right">{{ totalSubKeyUsage.cacheCreation.toLocaleString() }}</q-td>
                        <q-td class="text-right">{{ totalSubKeyUsage.cacheRead.toLocaleString() }}</q-td>
                        <q-td class="text-right">{{ totalSubKeyUsage.requests.toLocaleString() }}</q-td>
                        <q-td class="text-right">{{ totalSubKeyUsage.peakTpm.toLocaleString() }}</q-td>
                        <q-td class="text-right">{{ totalSubKeyUsage.cost != null ? `$${totalSubKeyUsage.cost.toFixed(4)}` : '\u2014' }}</q-td>
                        <q-td />
                        <q-td />
                      </q-tr>
                    </template>
                  </q-table>
                </q-card-section>
              </q-card>
            </q-tab-panel>
          </q-tab-panels>
        </q-tab-panel>

        <!-- Spam Tab -->
        <q-tab-panel name="spam" class="q-pa-none">
          <q-card flat bordered>
            <q-card-section>
              <div class="row items-center q-mb-sm">
                <div class="text-subtitle1">Spam Detection</div>
                <q-space />
                <q-btn flat dense icon="refresh" @click="loadSpam" />
              </div>
              <div v-if="spamLoading && !spamRows.length" class="flex flex-center q-pa-lg">
                <q-spinner size="md" />
              </div>
              <q-banner v-else-if="spamError" class="bg-negative text-white q-mb-sm" rounded>
                {{ spamError }}
                <template #action>
                  <q-btn flat label="Retry" @click="loadSpam" />
                </template>
              </q-banner>
              <q-table
                v-else
                flat bordered dense
                :rows="spamRows"
                :columns="spamColumns"
                :row-key="(row) => `${row.group_key_id}-${row.spam_type}`"
                v-model:pagination="spamPagination"
                :loading="spamLoading"
                @request="onSpamRequest"
              >
                <template #body-cell-spam_type="props">
                  <q-td :props="props">
                    <q-badge
                      :color="props.row.spam_type === 'low_token' ? 'orange' : 'red'"
                      :label="props.row.spam_type === 'low_token' ? 'Low Token' : 'Duplicate Request'"
                    />
                  </q-td>
                </template>
                <template #body-cell-api_key="props">
                  <q-td :props="props">
                    <code>{{ props.row.api_key }}</code>
                    <q-btn flat dense size="xs" icon="content_copy" aria-label="Copy key" @click.stop="copyText(props.row.api_key)" />
                  </q-td>
                </template>
                <template #no-data>
                  <div class="full-width text-center q-pa-md text-grey">No spam detected for this group</div>
                </template>
              </q-table>
            </q-card-section>
          </q-card>
        </q-tab-panel>

        <!-- User Agents Tab -->
        <q-tab-panel name="user-agents" class="q-pa-none">
          <q-card flat bordered>
            <q-card-section>
              <div class="text-subtitle1 q-mb-sm">Blocked User Agents</div>
              <div v-if="uaLoading" class="flex flex-center q-pa-lg">
                <q-spinner size="md" />
              </div>
              <template v-else>
                <div class="row items-center q-gutter-sm q-mb-md">
                  <q-select
                    v-model="selectedUa"
                    :options="filteredUaOptions"
                    label="User Agent"
                    outlined
                    dense
                    use-input
                    clearable
                    input-debounce="0"
                    class="col"
                    @filter="onUaFilter"
                    @new-value="onUaNewValue"
                  />
                  <q-btn color="primary" label="Add" :disable="!selectedUa" :loading="addingBlockedUa" @click="onAddBlockedUa" />
                </div>
                <div v-if="blockedUserAgents.length === 0" class="text-grey q-pa-sm">
                  No blocked user agents
                </div>
                <div v-else class="q-gutter-xs">
                  <q-chip
                    v-for="ua in blockedUserAgents"
                    :key="ua.user_agent"
                    :removable="removingBlockedUa !== ua.user_agent"
                    color="negative"
                    text-color="white"
                    @remove="onRemoveBlockedUa(ua.user_agent)"
                  >
                    <q-spinner v-if="removingBlockedUa === ua.user_agent" size="xs" class="q-mr-xs" />
                    {{ ua.user_agent }}
                  </q-chip>
                </div>
              </template>
            </q-card-section>
          </q-card>
        </q-tab-panel>
      </q-tab-panels>

      <q-dialog v-model="showAddServer" @hide="resetAddForm">
        <q-card style="width: 400px">
          <q-card-section><div class="text-h6">Add Server</div></q-card-section>
          <q-card-section>
            <q-select
              v-model="addForm.server_id"
              :options="addServerOptions"
              label="Server"
              outlined
              emit-value
              map-options
              class="q-mb-sm"
            />
            <template v-if="isCreatingNew">
              <q-input v-model="newServerForm.name" label="Name" outlined class="q-mb-sm" />
              <q-input v-model="newServerForm.base_url" label="Base URL" outlined class="q-mb-sm" />
              <q-input v-model="newServerForm.api_key" label="API Key (optional)" outlined class="q-mb-sm" />
            </template>
            <q-input v-model.number="addForm.priority" label="Priority" type="number" outlined />
          </q-card-section>
          <q-card-actions align="right">
            <q-btn flat label="Cancel" v-close-popup />
            <q-btn color="primary" label="Add" :loading="addingServer" @click="onAddServer" />
          </q-card-actions>
        </q-card>
      </q-dialog>

      <q-dialog v-model="showMappings">
        <q-card style="width: 500px">
          <q-card-section><div class="text-h6">Model Mappings — {{ editingMapping?.server_name }}</div></q-card-section>
          <q-card-section>
            <div v-for="(entry, idx) in mappingEntries" :key="idx" class="row q-gutter-sm q-mb-sm">
              <q-input v-model="entry.from" label="From model" outlined dense style="flex:1" />
              <q-input v-model="entry.to" label="To model" outlined dense style="flex:1" />
              <q-btn flat dense icon="close" @click="mappingEntries.splice(idx, 1)" />
            </div>
            <q-btn flat dense icon="add" label="Add mapping" @click="mappingEntries.push({ from: '', to: '' })" />
          </q-card-section>
          <q-card-actions align="right">
            <q-btn flat label="Cancel" v-close-popup />
            <q-btn color="primary" label="Save" @click="saveMappings" />
          </q-card-actions>
        </q-card>
      </q-dialog>

      <q-dialog v-model="showEditServer">
        <q-card style="width: 400px">
          <q-card-section><div class="text-h6">Edit Server</div></q-card-section>
          <q-card-section>
            <q-input v-model="editServerForm.name" label="Name" outlined class="q-mb-sm" />
            <q-input v-model="editServerForm.base_url" label="Base URL" outlined class="q-mb-sm" />
            <q-input v-model="editServerForm.api_key" label="API Key (optional)" outlined class="q-mb-sm" />
            <q-input v-model="editServerForm.system_prompt" label="System Prompt (optional)" type="textarea" outlined class="q-mb-sm" />
            <q-toggle v-model="editServerForm.remove_thinking" label="Remove Thinking" class="q-mt-sm" />
            <div class="text-subtitle2 q-mt-md q-mb-xs">Custom Headers</div>
            <div v-for="(entry, idx) in editServerCustomHeaders" :key="idx" class="row q-gutter-sm q-mb-sm">
              <q-input v-model="entry.name" label="Header name" outlined dense style="flex:1" />
              <q-input v-model="entry.value" label="Header value" outlined dense style="flex:1" />
              <q-btn flat dense icon="close" @click="editServerCustomHeaders.splice(idx, 1)" />
            </div>
            <div class="row q-gutter-sm">
              <q-btn flat dense icon="add" label="Add header" @click="editServerCustomHeaders.push({ name: '', value: '' })" />
              <q-btn flat dense icon="add" label="anthropic-workspace-id" @click="addEditServerWorkspaceIdHeader" />
            </div>
            <div class="text-subtitle2 q-mt-md q-mb-xs">Circuit Breaker</div>
            <div class="text-caption text-grey q-mb-sm">
              Auto-shutdown the server when errors exceed a threshold, then auto-restart after a cooldown period. Leave all fields empty to disable.
            </div>
            <q-input
              v-model.number="editServerCbForm.cb_max_failures"
              label="Max Failures"
              type="number"
              :min="1"
              outlined
              dense
              clearable
              class="q-mb-sm"
              @clear="onCbFieldClear('cb_max_failures')"
            />
            <q-input
              v-model.number="editServerCbForm.cb_window_seconds"
              label="Failure Window (seconds)"
              type="number"
              :min="1"
              outlined
              dense
              clearable
              class="q-mb-sm"
              @clear="onCbFieldClear('cb_window_seconds')"
            />
            <q-input
              v-model.number="editServerCbForm.cb_cooldown_seconds"
              label="Cooldown (seconds)"
              type="number"
              :min="1"
              outlined
              dense
              clearable
              class="q-mb-sm"
              @clear="onCbFieldClear('cb_cooldown_seconds')"
            />
            <div class="text-subtitle2 q-mt-md q-mb-xs">Rate Limit</div>
            <div class="text-caption text-grey q-mb-sm">
              Limit how many requests this server receives within a time window. Leave both fields empty to disable.
            </div>
            <q-input
              v-model.number="editServerRlForm.max_requests"
              label="Max Requests"
              type="number"
              :min="1"
              outlined
              dense
              clearable
              class="q-mb-sm"
              @clear="onRlFieldClear()"
            />
            <q-input
              v-model.number="editServerRlForm.rate_window_seconds"
              label="Window (seconds)"
              type="number"
              :min="1"
              outlined
              dense
              clearable
              class="q-mb-sm"
              @clear="onRlFieldClear()"
            />
            <div class="text-subtitle2 q-mt-md q-mb-xs">Per-Key Rate Limit</div>
            <div class="text-caption text-grey q-mb-sm">
              Limit requests per sub-key to this server within a time window. Window accepts shorthand like 1s, 1m, 1h, 1d, 1w. Leave both empty to disable.
            </div>
            <q-input
              v-model.number="editServerPkRlForm.per_key_max_requests"
              label="Max Requests per Key"
              type="number"
              :min="1"
              outlined
              dense
              clearable
              class="q-mb-sm"
              @clear="onPkRlFieldClear()"
            />
            <q-input
              v-model="editServerPkRlForm.per_key_window_str"
              label="Window (e.g. 1h, 30m, 1d)"
              outlined
              dense
              clearable
              class="q-mb-sm"
              :error="!!editServerPkRlForm.per_key_window_error"
              :error-message="editServerPkRlForm.per_key_window_error"
              @clear="onPkRlFieldClear()"
            />
            <div class="text-subtitle2 q-mt-md q-mb-xs">Max Input Tokens</div>
            <div class="text-caption text-grey q-mb-sm">
              Skip this server when the estimated input token count exceeds this limit (approximate). Leave empty to disable.
            </div>
            <q-input
              v-model.number="editServerTokenForm.max_input_tokens"
              label="Max Input Tokens"
              type="number"
              :min="1"
              outlined
              dense
              clearable
              class="q-mb-sm"
              @clear="editServerTokenForm.max_input_tokens = null"
            />
            <div class="text-subtitle2 q-mt-md q-mb-xs">Min Input Tokens</div>
            <div class="text-caption text-grey q-mb-sm">
              Skip this server when the estimated input token count is below this limit (approximate). Leave empty to disable.
            </div>
            <q-input
              v-model.number="editServerTokenForm.min_input_tokens"
              label="Min Input Tokens"
              type="number"
              :min="1"
              outlined
              dense
              clearable
              class="q-mb-sm"
              @clear="editServerTokenForm.min_input_tokens = null"
            />
            <div class="text-subtitle2 q-mt-md q-mb-xs">Supported Models</div>
            <div class="text-caption text-grey q-mb-sm">
              Restrict this server to specific models. Leave empty to accept all models.
            </div>
            <q-select
              v-model="editServerSupportedModels"
              :options="editServerModelOptions"
              label="Supported Models"
              outlined
              dense
              multiple
              use-chips
              use-input
              input-debounce="200"
              new-value-mode="add-unique"
              class="q-mb-sm"
              @filter="onEditServerModelFilter"
            />
            <div class="text-subtitle2 q-mt-md q-mb-xs">Active Hours</div>
            <div class="text-caption text-grey q-mb-sm">
              Leave empty for 24/7. Overnight windows supported (e.g., 22:00-06:00).
            </div>
            <q-select
              v-model="editServerActiveHoursForm.timezone"
              :options="filteredTimezoneOptions"
              label="Timezone"
              outlined
              dense
              clearable
              use-input
              input-debounce="0"
              class="q-mb-sm"
              :error="!activeHoursValid"
              :error-message="activeHoursErrorMessage"
              @filter="filterTimezones"
            />
            <div class="row q-gutter-sm q-mb-sm">
              <q-input
                v-model="editServerActiveHoursForm.start"
                label="Start Time"
                outlined
                dense
                mask="##:##"
                placeholder="HH:MM"
                style="flex: 1"
                clearable
                @clear="editServerActiveHoursForm.start = ''"
              />
              <q-input
                v-model="editServerActiveHoursForm.end"
                label="End Time"
                outlined
                dense
                mask="##:##"
                placeholder="HH:MM"
                style="flex: 1"
                clearable
                @clear="editServerActiveHoursForm.end = ''"
              />
            </div>
            <q-btn
              flat
              dense
              label="Clear Active Hours"
              size="sm"
              class="q-mb-sm"
              @click="onActiveHoursClear"
            />
          </q-card-section>
          <q-card-actions align="right">
            <q-btn flat label="Cancel" v-close-popup />
            <q-btn color="primary" label="Save" :loading="savingServer" :disable="!activeHoursValid" @click="onSaveEditServer" />
          </q-card-actions>
        </q-card>
      </q-dialog>

      <q-dialog v-model="showCreateKey" @hide="newKeyName = ''">
        <q-card style="width: 400px">
          <q-card-section><div class="text-h6">Create Sub-Key</div></q-card-section>
          <q-card-section>
            <q-input v-model="newKeyName" label="Name" outlined maxlength="100" />
          </q-card-section>
          <q-card-actions align="right">
            <q-btn flat label="Cancel" v-close-popup />
            <q-btn color="primary" label="Create" :loading="creatingKey" @click="onCreateKey" />
          </q-card-actions>
        </q-card>
      </q-dialog>

      <!-- Assign Subscription Dialog -->
      <q-dialog v-model="showAssignSubDialog" persistent>
        <q-card style="min-width: 380px">
          <q-card-section class="row items-center q-pb-none">
            <div class="text-h6">Add Subscription</div>
            <q-space />
            <q-btn icon="close" flat round dense v-close-popup />
          </q-card-section>
          <q-card-section class="q-gutter-sm">
            <q-select
              v-model="assignSubPlanId"
              :options="activePlans.map((p) => ({ label: `${p.name} · ${getSubTypeLabel(p.sub_type)} · $${p.cost_limit_usd.toFixed(2)}`, value: p.id }))"
              label="Plan *"
              emit-value
              map-options
              outlined
              dense
            />
            <q-input
              :model-value="assignSubExpiryDate"
              label="Custom expiry date (optional)"
              outlined
              dense
              readonly
              clearable
              @clear="clearAssignSubExpiry"
            >
              <template #append>
                <q-icon name="event" class="cursor-pointer">
                  <q-popup-proxy transition-show="scale" transition-hide="scale">
                    <q-date
                      :model-value="assignSubExpiryModel"
                      @update:model-value="onAssignSubDatePick"
                    />
                  </q-popup-proxy>
                </q-icon>
              </template>
            </q-input>
          </q-card-section>
          <q-card-actions align="right">
            <q-btn flat label="Cancel" v-close-popup />
            <q-btn
              label="Assign"
              color="primary"
              :disable="!assignSubPlanId"
              @click="onAssignSubscription(assignSubKeyId, assignSubPlanId, assignSubExpiryDate)"
            />
          </q-card-actions>
        </q-card>
      </q-dialog>

      <!-- Bulk Create Keys Dialog -->
      <q-dialog v-model="showBulkCreate" persistent>
        <q-card style="min-width: 380px">
          <q-card-section class="row items-center q-pb-none">
            <div class="text-h6">Bulk Create Sub-Keys</div>
            <q-space />
            <q-btn icon="close" flat round dense v-close-popup />
          </q-card-section>
          <q-card-section class="q-gutter-sm">
            <q-select
              v-model="bulkCreateForm.plan_id"
              :options="activePlans.map((p) => ({ label: `${p.name} · ${getSubTypeLabel(p.sub_type)} · $${p.cost_limit_usd.toFixed(2)}`, value: p.id }))"
              label="Plan *"
              emit-value
              map-options
              outlined
              dense
            />
            <q-input
              :model-value="bulkCreateForm.custom_expires_at"
              label="Custom expiry date (optional)"
              outlined
              dense
              readonly
              clearable
              @clear="clearBulkCreateExpiry"
            >
              <template #append>
                <q-icon name="event" class="cursor-pointer">
                  <q-popup-proxy transition-show="scale" transition-hide="scale">
                    <q-date
                      :model-value="bulkCreateForm.custom_expires_model"
                      @update:model-value="onBulkCreateDatePick"
                    />
                  </q-popup-proxy>
                </q-icon>
              </template>
            </q-input>
            <q-input
              v-model.number="bulkCreateForm.count"
              label="Count *"
              type="number"
              outlined
              dense
              :rules="[(v) => (v >= 1 && v <= 500) || 'Must be 1–500']"
            />
            <q-input
              v-model="bulkCreateForm.name_prefix"
              label="Name prefix (optional)"
              outlined
              dense
              hint="e.g. 'team-a' → team-a-PlanName-1, team-a-PlanName-2, ..."
            />
          </q-card-section>
          <q-card-actions align="right">
            <q-btn flat label="Cancel" v-close-popup />
            <q-btn
              label="Create"
              color="primary"
              :loading="bulkCreating"
              :disable="!bulkCreateForm.plan_id || bulkCreateForm.count < 1 || bulkCreateForm.count > 500"
              @click="onBulkCreate"
            />
          </q-card-actions>
        </q-card>
      </q-dialog>

      <!-- Bulk Create Result Dialog -->
      <q-dialog v-model="showBulkResult">
        <q-card style="min-width: 340px">
          <q-card-section class="row items-center q-pb-none">
            <div class="text-h6">Keys Created</div>
            <q-space />
            <q-btn icon="close" flat round dense v-close-popup />
          </q-card-section>
          <q-card-section>
            <div class="text-body2">{{ bulkCreatedKeys.length }} sub-keys created successfully.</div>
          </q-card-section>
          <q-card-actions align="right">
            <q-btn flat label="Close" v-close-popup />
            <q-btn label="Download CSV" color="primary" icon="download" @click="downloadBulkCsv" />
          </q-card-actions>
        </q-card>
      </q-dialog>

      <q-dialog v-model="showRateModal">
        <q-card style="width: 400px">
          <q-card-section><div class="text-h6">Cost Rates — {{ rateEditServer?.server_name }}</div></q-card-section>
          <q-card-section class="q-gutter-sm">
            <q-input v-model.number="rateForm.rate_input" label="Input Rate" outlined dense type="number" :min="0" placeholder="1.0" clearable @clear="rateForm.rate_input = null" />
            <q-input v-model.number="rateForm.rate_output" label="Output Rate" outlined dense type="number" :min="0" placeholder="1.0" clearable @clear="rateForm.rate_output = null" />
            <q-input v-model.number="rateForm.rate_cache_write" label="Cache Write Rate" outlined dense type="number" :min="0" placeholder="1.0" clearable @clear="rateForm.rate_cache_write = null" />
            <q-input v-model.number="rateForm.rate_cache_read" label="Cache Read Rate" outlined dense type="number" :min="0" placeholder="1.0" clearable @clear="rateForm.rate_cache_read = null" />
            <q-toggle v-model="rateForm.normalize_cache_read" label="Normalize Cache Read" dense />
          </q-card-section>
          <q-card-actions align="right">
            <q-btn flat label="Cancel" v-close-popup />
            <q-btn color="primary" label="Save" :loading="savingRate" @click="onSaveRates" />
          </q-card-actions>
        </q-card>
      </q-dialog>

      <q-dialog v-model="showRetryDialog">
        <q-card style="width: 420px">
          <q-card-section><div class="text-h6">Retry &amp; Timeout — {{ retryEditServer?.server_name }}</div></q-card-section>
          <q-card-section class="q-gutter-sm">
            <q-input
              v-model="retryForm.retry_status_codes_str"
              label="Retry Status Codes (comma-separated, e.g. 503,429)"
              outlined
              dense
              placeholder="503,429"
              clearable
              @clear="retryForm.retry_status_codes_str = ''"
            />
            <q-input
              v-model.number="retryForm.retry_count"
              label="Retry Count"
              outlined
              dense
              type="number"
              :min="1"
              placeholder="2"
              clearable
              @clear="retryForm.retry_count = null"
            />
            <q-input
              v-model.number="retryForm.retry_delay_seconds"
              label="Retry Delay (seconds)"
              outlined
              dense
              type="number"
              :min="0.1"
              :step="0.1"
              placeholder="1.0"
              clearable
              @clear="retryForm.retry_delay_seconds = null"
            />
            <div class="text-caption text-grey">Leave all fields empty to disable retry for this server.</div>
            <q-separator class="q-my-sm" />
            <q-input
              v-model.number="retryForm.non_stream_timeout_ms"
              label="Non-streaming Timeout (ms, empty = disabled)"
              outlined
              dense
              type="number"
              :min="1"
              placeholder="60000"
              clearable
              hint="Fail over to the next server if a non-streaming /v1/messages call takes longer"
              @clear="retryForm.non_stream_timeout_ms = null"
            />
          </q-card-section>
          <q-card-actions align="right">
            <q-btn flat label="Cancel" v-close-popup />
            <q-btn color="primary" label="Save" :loading="savingRetry" @click="saveRetryConfig" />
          </q-card-actions>
        </q-card>
      </q-dialog>

      <!-- Edit Bonus Allowed Models Dialog -->
      <q-dialog v-model="showEditBonusModelsDialog" persistent>
        <q-card style="width: 480px">
          <q-card-section>
            <div class="text-h6">Edit Allowed Models</div>
            <div class="text-caption text-grey">Leave empty to allow all models.</div>
          </q-card-section>
          <q-card-section>
            <q-select
              v-model="editBonusModelsForm.bonus_allowed_models"
              label="Allowed Models"
              outlined
              dense
              multiple
              use-chips
              emit-value
              map-options
              :options="bonusAllowedModelOptions"
            />
          </q-card-section>
          <q-card-actions align="right">
            <q-btn flat label="Cancel" v-close-popup />
            <q-btn
              color="primary"
              label="Save"
              :loading="editBonusModelsLoading"
              @click="onSaveBonusAllowedModels"
            />
          </q-card-actions>
        </q-card>
      </q-dialog>

      <!-- Edit Bonus Headers Dialog -->
      <q-dialog v-model="showEditBonusHeadersDialog" persistent>
        <q-card style="width: 520px; max-width: 90vw">
          <q-card-section>
            <div class="text-h6">Edit Bonus Config</div>
            <div class="text-caption text-grey">Quota URL/headers (used for quota check), custom headers (forwarded to upstream), and the non-streaming timeout.</div>
          </q-card-section>
          <q-card-section class="q-gutter-sm">
            <q-input
              v-model="editBonusHeadersForm.bonus_quota_url"
              label="Quota Check URL (optional)"
              outlined
              dense
              placeholder="https://example.com/quota"
            />
            <q-input
              v-model.number="editBonusHeadersForm.bonus_non_stream_timeout_ms"
              label="Non-streaming Timeout (ms, empty = use global default)"
              outlined
              dense
              type="number"
              :min="1"
              clearable
              hint="Move to the next bonus server if a non-streaming call takes longer"
              @clear="editBonusHeadersForm.bonus_non_stream_timeout_ms = null"
            />
            <div>
              <div class="text-subtitle2 q-mb-xs">Quota Headers</div>
              <div v-for="(entry, idx) in editBonusQuotaHeaders" :key="`eq-${idx}`" class="row q-gutter-sm q-mb-sm">
                <q-input v-model="entry.name" label="Header name" outlined dense style="flex:1" />
                <q-input v-model="entry.value" label="Header value" outlined dense style="flex:1" />
                <q-btn flat dense icon="close" @click="editBonusQuotaHeaders.splice(idx, 1)" />
              </div>
              <q-btn flat dense icon="add" label="Add header" @click="editBonusQuotaHeaders.push({ name: '', value: '' })" />
            </div>
            <div>
              <div class="text-subtitle2 q-mb-xs">Custom Headers (forwarded to upstream)</div>
              <div v-for="(entry, idx) in editBonusCustomHeaders" :key="`ec-${idx}`" class="row q-gutter-sm q-mb-sm">
                <q-input v-model="entry.name" label="Header name" outlined dense style="flex:1" />
                <q-input v-model="entry.value" label="Header value" outlined dense style="flex:1" />
                <q-btn flat dense icon="close" @click="editBonusCustomHeaders.splice(idx, 1)" />
              </div>
              <q-btn flat dense icon="add" label="Add header" @click="editBonusCustomHeaders.push({ name: '', value: '' })" />
            </div>
          </q-card-section>
          <q-card-actions align="right">
            <q-btn flat label="Cancel" v-close-popup />
            <q-btn
              color="primary"
              label="Save"
              :loading="editBonusHeadersLoading"
              @click="onSaveBonusHeaders"
            />
          </q-card-actions>
        </q-card>
      </q-dialog>

      <!-- Add Bonus Subscription Dialog -->
      <q-dialog v-model="showAddBonusDialog" persistent>
        <q-card style="width: 480px">
          <q-card-section>
            <div class="text-h6">Add Bonus Subscription</div>
          </q-card-section>
          <q-card-section class="q-gutter-sm">
            <q-input
              v-model="addBonusForm.bonus_name"
              label="Name *"
              outlined
              dense
              placeholder="My Bonus Server"
            />
            <q-input
              v-model="addBonusForm.bonus_base_url"
              label="Base URL *"
              outlined
              dense
              placeholder="https://api.anthropic.com"
            />
            <q-input
              v-model="addBonusForm.bonus_api_key"
              label="API Key *"
              outlined
              dense
              type="password"
              placeholder="sk-ant-..."
            />
            <q-input
              v-model="addBonusForm.bonus_quota_url"
              label="Quota Check URL (optional)"
              outlined
              dense
              placeholder="https://example.com/quota"
            />
            <div>
              <div class="text-subtitle2 q-mb-xs">Quota Headers (optional)</div>
              <div v-for="(entry, idx) in addBonusQuotaHeaders" :key="`q-${idx}`" class="row q-gutter-sm q-mb-sm">
                <q-input v-model="entry.name" label="Header name" outlined dense style="flex:1" />
                <q-input v-model="entry.value" label="Header value" outlined dense style="flex:1" />
                <q-btn flat dense icon="close" @click="addBonusQuotaHeaders.splice(idx, 1)" />
              </div>
              <q-btn flat dense icon="add" label="Add header" @click="addBonusQuotaHeaders.push({ name: '', value: '' })" />
            </div>
            <div>
              <div class="text-subtitle2 q-mb-xs">Custom Headers (forwarded to upstream)</div>
              <div v-for="(entry, idx) in addBonusCustomHeaders" :key="`c-${idx}`" class="row q-gutter-sm q-mb-sm">
                <q-input v-model="entry.name" label="Header name" outlined dense style="flex:1" />
                <q-input v-model="entry.value" label="Header value" outlined dense style="flex:1" />
                <q-btn flat dense icon="close" @click="addBonusCustomHeaders.splice(idx, 1)" />
              </div>
              <q-btn flat dense icon="add" label="Add header" @click="addBonusCustomHeaders.push({ name: '', value: '' })" />
            </div>
            <q-select
              v-model="addBonusForm.bonus_allowed_models"
              label="Allowed Models (optional)"
              outlined
              dense
              multiple
              use-chips
              emit-value
              map-options
              :options="bonusAllowedModelOptions"
              hint="Leave empty to allow all models"
            />
          </q-card-section>
          <q-card-actions align="right">
            <q-btn flat label="Cancel" v-close-popup />
            <q-btn color="primary" label="Add Bonus" :loading="addBonusLoading" @click="onSubmitAddBonus" />
          </q-card-actions>
        </q-card>
      </q-dialog>
    </div>
    <div v-else class="flex flex-center" style="min-height: 200px">
      <q-spinner size="lg" />
    </div>
  </q-page>
</template>

<script setup lang="ts">
import { ref, computed, onMounted, onUnmounted, watch } from 'vue';
import { useRoute, useRouter } from 'vue-router';
import { useQuasar, copyToClipboard } from 'quasar';
import { useGroupsStore, type GroupWithServers, type GroupServerDetail, type CircuitStatus, type TokenUsageStats, type GroupKey, type Model, type SpamResult, type KeyTokenUsage } from 'stores/groups';
import { useServersStore } from 'stores/servers';
import { useModelsStore } from 'stores/models';
import { api } from 'boot/axios';
import { subKeyUsageColumns as subKeyUsageColumnsDef } from 'src/utils/subKeyUsageColumns';
import SubKeyUsage from 'components/SubKeyUsage.vue';
import TtftChart from 'components/TtftChart.vue';
import UptimeBars from 'components/UptimeBars.vue';
import type { Bucket } from 'components/UptimeBars.vue';
import { getSubTypeLabel } from 'src/composables/useSubscriptionType';

const $q = useQuasar();
const route = useRoute();
const router = useRouter();
const groupsStore = useGroupsStore();
const serversStore = useServersStore();
const modelsStore = useModelsStore();

const group = ref<GroupWithServers | null>(null);
const servers = ref<GroupServerDetail[]>([]);
const failoverCodesStr = ref('');
const validTabs = ['properties', 'servers', 'keys', 'allowed-models', 'ttft', 'token-usage', 'spam', 'user-agents'];
const initialTab = validTabs.includes(route.query.tab as string) ? (route.query.tab as string) : 'properties';
const activeTab = ref(initialTab);

const CREATE_NEW = '__create_new__';
const showAddServer = ref(false);
const addForm = ref({ server_id: '', priority: 1 });
const newServerForm = ref({ name: '', base_url: '', api_key: '' });
const addingServer = ref(false);
const isCreatingNew = computed(() => addForm.value.server_id === CREATE_NEW);
const addServerOptions = computed(() => [
  ...availableServers.value,
  { label: '＋ Create new server...', value: CREATE_NEW },
]);
const showMappings = ref(false);
const editingMapping = ref<GroupServerDetail | null>(null);
const mappingEntries = ref<{ from: string; to: string }[]>([]);

const showEditServer = ref(false);
const editServerId = ref('');
const editServerForm = ref({ name: '', base_url: '', api_key: '', system_prompt: '', remove_thinking: false });
const editServerCustomHeaders = ref<{ name: string; value: string }[]>([]);

function addEditServerWorkspaceIdHeader() {
  if (editServerCustomHeaders.value.some((e) => e.name === 'anthropic-workspace-id')) return;
  editServerCustomHeaders.value.push({ name: 'anthropic-workspace-id', value: '' });
}

const editServerCbForm = ref({
  cb_max_failures: null as number | null,
  cb_window_seconds: null as number | null,
  cb_cooldown_seconds: null as number | null,
});
const editServerRlForm = ref({
  max_requests: null as number | null,
  rate_window_seconds: null as number | null,
});
const editServerPkRlForm = ref({
  per_key_max_requests: null as number | null,
  per_key_window_str: '' as string,
  per_key_window_error: '' as string,
});
const editServerTokenForm = ref({
  max_input_tokens: null as number | null,
  min_input_tokens: null as number | null,
});
const editServerSupportedModels = ref<string[]>([]);
const editServerModelOptions = ref<string[]>([]);
const savingServer = ref(false);

const IANA_TIMEZONE_LIST = [
  'UTC',
  'US/Eastern',
  'US/Central',
  'US/Mountain',
  'US/Pacific',
  'Europe/London',
  'Europe/Paris',
  'Europe/Berlin',
  'Asia/Tokyo',
  'Asia/Shanghai',
  'Asia/Ho_Chi_Minh',
  'Asia/Bangkok',
  'Asia/Singapore',
  'Australia/Sydney',
  'America/Sao_Paulo',
];

const editServerActiveHoursForm = ref({
  timezone: '' as string | null,
  start: '',
  end: '',
});
const filteredTimezoneOptions = ref<string[]>(IANA_TIMEZONE_LIST);

function filterTimezones(val: string, update: (fn: () => void) => void) {
  update(() => {
    if (!val) {
      filteredTimezoneOptions.value = IANA_TIMEZONE_LIST;
    } else {
      const lower = val.toLowerCase();
      filteredTimezoneOptions.value = IANA_TIMEZONE_LIST.filter((tz) =>
        tz.toLowerCase().includes(lower),
      );
    }
  });
}

function onActiveHoursClear() {
  editServerActiveHoursForm.value.timezone = null;
  editServerActiveHoursForm.value.start = '';
  editServerActiveHoursForm.value.end = '';
}

const activeHoursValid = computed(() => {
  const { timezone, start, end } = editServerActiveHoursForm.value;
  const hasTimezone = !!timezone;
  const hasStart = !!start;
  const hasEnd = !!end;
  const filledCount = [hasTimezone, hasStart, hasEnd].filter(Boolean).length;
  // All three filled or all three empty is valid
  return filledCount === 0 || filledCount === 3;
});

const activeHoursErrorMessage = computed(() => {
  if (activeHoursValid.value) return '';
  return 'All three active hours fields must be filled or all left empty.';
});

const keyBuilderEntries = ref<{ server_id: string; server_name: string; short_id: number; key: string; defaultKey: string }[]>([]);
const showAllKeyBuilderServers = ref(false);

const ttftTimeoutStr = ref('');
const ttftKeyFilter = ref<string | null>(null);
const ctMappingEntries = ref<{ from: string; to: string }[]>([]);
const circuitStatuses = ref<CircuitStatus[]>([]);
let circuitPollTimer: ReturnType<typeof setInterval> | null = null;

// Token usage state
const tokenUsageStats = ref<TokenUsageStats | null>(null);
const tokenUsageLoading = ref(false);
const tokenUsageError = ref('');
const tokenUsagePeriod = ref('today');
const tokenUsageServerFilter = ref<string | null>(null);
const tokenUsageDynamicKeyFilter = ref(false);
const tokenUsageKeyHashFilter = ref('');

// Token usage child tabs
const tokenUsageChildTab = ref('by-server');

// By Sub-Key usage state
const subKeyUsageData = ref<KeyTokenUsage[]>([]);
const subKeyUsageLoading = ref(false);
const subKeyUsageError = ref('');
const subKeyDateStart = ref(
  new Date(Date.now() - 30 * 24 * 60 * 60 * 1000).toISOString().slice(0, 10),
);
const subKeyDateEnd = ref(new Date().toISOString().slice(0, 10));
const subKeyUsageExpanded = ref<string[]>([]);

// Spam detection state
const spamRows = ref<SpamResult[]>([]);
const spamLoading = ref(false);
const spamError = ref('');
const spamPagination = ref({ page: 1, rowsPerPage: 20, rowsNumber: 0, sortBy: '', descending: false });

// User agents state
const userAgents = ref<{ user_agent: string; first_seen_at: string }[]>([]);
const blockedUserAgents = ref<{ user_agent: string; created_at: string }[]>([]);
const uaLoading = ref(false);
const selectedUa = ref<string | null>(null);
const addingBlockedUa = ref(false);
const removingBlockedUa = ref<string | null>(null);
const uaFilterText = ref('');

// Sub-keys state
const subKeys = ref<GroupKey[]>([]);
const subKeysLoading = ref(false);
const subKeySearch = ref('');
const subKeyError = ref('');
const subKeyPagination = ref({ page: 1, rowsPerPage: 50, rowsNumber: 0, sortBy: '', descending: false });
const showCreateKey = ref(false);
const newKeyName = ref('');
const creatingKey = ref(false);

const showBulkCreate = ref(false);
const bulkCreateForm = ref({ plan_id: '', count: 10, name_prefix: '', custom_expires_at: '', custom_expires_model: '' });
const bulkCreating = ref(false);
const bulkCreatedKeys = ref<GroupKey[]>([]);
const showBulkResult = ref(false);

// Subscription state
interface KeySubscription {
  id: string;
  group_key_id: string;
  plan_id: string | null;
  sub_type: string;
  cost_limit_usd: number;
  weekly_cost_limit_usd: number | null;
  model_limits: Record<string, number>;
  reset_hours: number | null;
  rpm_limit: number | null;
  tpm_limit: number | null;
  duration_days: number;
  status: string;
  activated_at: string | null;
  expires_at: string | null;
  created_at: string;
  cost_used: number;
  bonus_name: string | null;
  bonus_base_url: string | null;
  bonus_api_key: string | null;
  bonus_quota_url: string | null;
  bonus_quota_headers: Record<string, string> | null;
  bonus_allowed_models?: string[] | null;
  bonus_custom_headers: Record<string, string> | null;
  sort_order: number;
  /** Per-bonus non-streaming timeout; null defers to the global default. */
  bonus_non_stream_timeout_ms: number | null;
}
interface SubscriptionPlan {
  id: string;
  name: string;
  sub_type: string;
  cost_limit_usd: number;
  weekly_cost_limit_usd: number | null;
  is_active: boolean;
}
const keySubscriptions = ref<Record<string, { data: KeySubscription[]; total: number }>>({});
const keySubsLoading = ref<Record<string, boolean>>({});
const subPagination = ref<Record<string, { page: number; rowsPerPage: number; rowsNumber: number }>>({});
const activePlans = ref<SubscriptionPlan[]>([]);

// Add Subscription dialog state
const showAssignSubDialog = ref(false);
const assignSubKeyId = ref('');
const assignSubPlanId = ref('');
const assignSubExpiryDate = ref('');
const assignSubExpiryModel = ref('');

// Add Bonus dialog state
const showAddBonusDialog = ref(false);
const addBonusKeyId = ref('');
const addBonusForm = ref({
  bonus_name: '',
  bonus_base_url: 'https://api.anthropic.com',
  bonus_api_key: '',
  bonus_quota_url: '',
  bonus_allowed_models: [] as string[],
});
const addBonusQuotaHeaders = ref<{ name: string; value: string }[]>([]);
const addBonusCustomHeaders = ref<{ name: string; value: string }[]>([]);
const addBonusLoading = ref(false);

// Edit Bonus Headers dialog state
const showEditBonusHeadersDialog = ref(false);
const editBonusHeadersKeyId = ref('');
const editBonusHeadersSubId = ref('');
const editBonusHeadersForm = ref({
  bonus_quota_url: '',
  bonus_non_stream_timeout_ms: null as number | null,
});
const editBonusQuotaHeaders = ref<{ name: string; value: string }[]>([]);
const editBonusCustomHeaders = ref<{ name: string; value: string }[]>([]);
const editBonusHeadersLoading = ref(false);

// Reorder bonus state
const reorderingBonusSubs = ref<Set<string>>(new Set());

// Edit Bonus allowed models dialog state
const showEditBonusModelsDialog = ref(false);
const editBonusModelsKeyId = ref('');
const editBonusModelsSubId = ref('');
const editBonusModelsForm = ref({
  bonus_allowed_models: [] as string[],
});
const editBonusModelsLoading = ref(false);

const bonusAllowedModelOptions = computed(() =>
  allowedModels.value.map((model) => ({ label: model.name, value: model.name })),
);

const subColumns = [
  { name: 'plan_name', label: 'Plan', field: (row: KeySubscription) => row.bonus_name ?? (row.plan_id ? 'Plan' : '—'), align: 'left' as const },
  { name: 'sub_type', label: 'Type', field: 'sub_type', align: 'left' as const, format: (v: string) => getSubTypeLabel(v) },
  { name: 'cost_limit_usd', label: 'Budget', field: 'cost_limit_usd', align: 'right' as const, format: (v: number) => `$${v.toFixed(2)}` },
  { name: 'weekly_cost_limit_usd', label: 'Weekly Limit', field: 'weekly_cost_limit_usd', align: 'right' as const, format: (v: number | null) => v != null ? `$${v.toFixed(2)}` : 'Unlimited' },
  { name: 'cost_used', label: 'Used', field: 'cost_used', align: 'right' as const },
  { name: 'bonus_allowed_models', label: 'Allowed Models', field: 'bonus_allowed_models', align: 'left' as const },
  { name: 'rpm_limit', label: 'RPM', field: 'rpm_limit', align: 'right' as const, format: (v: number | null) => v != null ? String(v) : '\u2014' },
  { name: 'tpm_limit', label: 'TPM Limit', field: 'tpm_limit', align: 'right' as const, format: (v: number | null) => v != null ? String(v) : '\u2014' },
  { name: 'status', label: 'Status', field: 'status', align: 'center' as const },
  { name: 'duration_days', label: 'Duration', field: 'duration_days', align: 'right' as const, format: (v: number) => `${v}d` },
  { name: 'actions', label: '', field: 'id', align: 'right' as const },
];

function subStatusColor(status: string): string {
  switch (status) {
    case 'active': return 'green';
    case 'exhausted': return 'red';
    case 'expired': return 'orange';
    case 'cancelled': return 'grey';
    default: return 'grey';
  }
}

// Rate modal state
const showRateModal = ref(false);

// Retry config dialog state
const showRetryDialog = ref(false);
const retryEditServer = ref<GroupServerDetail | null>(null);
const savingRetry = ref(false);
const retryForm = ref({
  retry_status_codes_str: '',
  retry_count: null as number | null,
  retry_delay_seconds: null as number | null,
  non_stream_timeout_ms: null as number | null,
});

// Uptime state
interface ServerUptime {
  server_id: string;
  server_name: string;
  buckets: Bucket[];
}
const uptimeData = ref<Record<string, Bucket[]>>({});
const uptimeLoading = ref(false);
const uptimeError = ref('');
const rateEditServer = ref<GroupServerDetail | null>(null);
const savingRate = ref(false);
const rateForm = ref({
  rate_input: null as number | null,
  rate_output: null as number | null,
  rate_cache_write: null as number | null,
  rate_cache_read: null as number | null,
  normalize_cache_read: false,
});

// Allowed models state
const allowedModels = ref<Model[]>([]);
const modelToAdd = ref<string | null>(null);
const modelSearchText = ref('');
const allModelsOptions = ref<{ label: string; value: string }[]>([]);
const filteredModelOptions = ref<{ label: string; value: string }[]>([]);
const allowedModelsLoading = ref(false);

// Key allowed models state
const keyAllowedModelsMap = ref<Record<string, Model[]>>({});
const keyModelsLoading = ref<Record<string, boolean>>({});

// Key server assignments state
const keyServersMap = ref<Record<string, GroupServerDetail[]>>({});
const keyServersLoading = ref<Record<string, boolean>>({});

const ttftKeyOptions = computed(() => [
  { label: 'All keys', value: null },
  ...subKeys.value.map((k) => ({ label: k.name, value: k.id })),
]);

const subKeyColumns = [
  { name: 'name', label: 'Name', field: 'name', align: 'left' as const },
  { name: 'api_key', label: 'Key', field: 'api_key', align: 'left' as const },
  { name: 'is_active', label: 'Status', field: 'is_active', align: 'center' as const },
  { name: 'actions', label: 'Actions', field: 'id', align: 'right' as const },
];

const tokenUsageServerOptions = computed(() =>
  servers.value.map((s) => ({ label: s.server_name, value: s.server_id })),
);

const formatCompact = (v: number) =>
  new Intl.NumberFormat('en', { notation: 'compact', maximumFractionDigits: 2 }).format(v);

const tokenUsageColumns = [
  { name: 'server', label: 'Server', field: 'server_name', align: 'left' as const },
  { name: 'model', label: 'Model', field: 'model', align: 'left' as const, format: (v: string | null) => v || '\u2014' },
  { name: 'input', label: 'Input Tokens', field: 'total_input_tokens', align: 'right' as const, format: formatCompact },
  { name: 'output', label: 'Output Tokens', field: 'total_output_tokens', align: 'right' as const, format: formatCompact },
  { name: 'cache_creation', label: 'Cache W', field: 'total_cache_creation_tokens', align: 'right' as const, format: formatCompact },
  { name: 'cache_read', label: 'Cache R', field: 'total_cache_read_tokens', align: 'right' as const, format: formatCompact },
  { name: 'requests', label: 'Requests', field: 'request_count', align: 'right' as const, format: formatCompact },
  { name: 'cost', label: 'Cost ($)', field: 'cost_usd', align: 'right' as const, format: (v: number | null) => v != null ? `$${v.toFixed(4)}` : '\u2014' },
];

const subKeyUsageColumns = subKeyUsageColumnsDef;

const totalSubKeyUsage = computed(() => {
  const rows = subKeyUsageData.value;
  const input = rows.reduce((s, r) => s + r.total_input_tokens, 0);
  const output = rows.reduce((s, r) => s + r.total_output_tokens, 0);
  const cacheCreation = rows.reduce((s, r) => s + r.total_cache_creation_tokens, 0);
  const cacheRead = rows.reduce((s, r) => s + r.total_cache_read_tokens, 0);
  const requests = rows.reduce((s, r) => s + r.request_count, 0);
  const peakTpm = rows.reduce((s, r) => Math.max(s, r.peak_tpm ?? 0), 0);
  const costRows = rows.filter((r) => r.cost_usd != null);
  const cost = costRows.length > 0 ? costRows.reduce((s, r) => s + (r.cost_usd ?? 0), 0) : null;
  return { input, output, cacheCreation, cacheRead, requests, peakTpm, cost };
});

const spamColumns = [
  { name: 'api_key', label: 'Key', field: 'api_key', align: 'left' as const },
  { name: 'key_name', label: 'Key Name', field: 'key_name', align: 'left' as const },
  { name: 'spam_type', label: 'Spam Type', field: 'spam_type', align: 'left' as const },
  { name: 'request_count', label: 'Request Count', field: 'request_count', align: 'right' as const },
  { name: 'peak_rpm', label: 'Peak RPM', field: 'peak_rpm', align: 'right' as const },
  {
    name: 'detected_at',
    label: 'Detected At',
    field: 'detected_at',
    align: 'left' as const,
    format: (v: string) => new Date(v).toLocaleString(),
  },
];

const filteredTokenUsageRows = computed(() => {
  if (!tokenUsageStats.value) return [];
  let rows = tokenUsageStats.value.servers;
  if (tokenUsageServerFilter.value) {
    rows = rows.filter((r) => r.server_id === tokenUsageServerFilter.value);
  }
  return rows.map((r, i) => ({ ...r, __key: `${r.server_id}-${r.model}-${i}` }));
});

const totalTokenUsage = computed(() => {
  const rows = filteredTokenUsageRows.value;
  const input = rows.reduce((s, r) => s + r.total_input_tokens, 0);
  const output = rows.reduce((s, r) => s + r.total_output_tokens, 0);
  const cacheCreation = rows.reduce((s, r) => s + r.total_cache_creation_tokens, 0);
  const cacheRead = rows.reduce((s, r) => s + r.total_cache_read_tokens, 0);
  const requests = rows.reduce((s, r) => s + r.request_count, 0);
  const costRows = rows.filter((r) => r.cost_usd != null);
  const cost = costRows.length > 0 ? costRows.reduce((s, r) => s + (r.cost_usd ?? 0), 0) : null;
  return { input, output, cacheCreation, cacheRead, requests, cost };
});
const visibleKeyBuilderEntries = computed(() =>
  showAllKeyBuilderServers.value
    ? keyBuilderEntries.value
    : keyBuilderEntries.value.filter((e) => !e.defaultKey)
);
const builtKey = computed(() => {
  if (!group.value) return '';
  const parts = [group.value.api_key];
  for (const e of keyBuilderEntries.value) {
    if (e.key) parts.push(`-rsv-${e.short_id}-${e.key}`);
  }
  return parts.length > 1 ? parts.join('') : '';
});

const allServers = ref<{ label: string; value: string }[]>([]);
const availableServers = computed(() =>
  allServers.value.filter((s) => !servers.value.some((gs) => gs.server_id === s.value))
);

onMounted(async () => {
  await loadGroup();
  const sData = await serversStore.fetchServers({ limit: 100 });
  if (sData) {
    allServers.value = sData.data.map((s) => ({ label: `${s.name} (#${s.short_id})`, value: s.id }));
  }
  loadTtftKeys();
  loadCircuitStatus();
  loadTokenUsage();
  loadUptime();
});

onUnmounted(() => {
  stopCircuitPoll();
});



async function loadGroup() {
  const id = route.params.id as string;
  const data = await groupsStore.getGroup(id);
  group.value = data;
  servers.value = data.servers;
  allowedModels.value = data.allowed_models || [];
  failoverCodesStr.value = (data.failover_status_codes || []).join(', ');
  ttftTimeoutStr.value = data.ttft_timeout_ms != null ? String(data.ttft_timeout_ms) : '';
  const ctm = data.count_tokens_model_mappings || {};
  ctMappingEntries.value = Object.entries(ctm).map(([from, to]) => ({ from, to }));
  keyBuilderEntries.value = data.servers.map((s) => ({
    server_id: s.server_id,
    server_name: s.server_name,
    short_id: s.short_id,
    key: '',
    defaultKey: serversStore.isProtected(s.server_id) && !serversStore.isUnlocked(s.server_id)
      ? '🔒 Protected'
      : (s.api_key || ''),
  }));
}

async function loadCircuitStatus() {
  if (!group.value) return;
  try {
    circuitStatuses.value = await groupsStore.fetchCircuitStatus(group.value.id);
  } catch {
    circuitStatuses.value = [];
  }
  // Start/stop polling based on whether any circuit is open
  if (circuitStatuses.value.length > 0) {
    startCircuitPoll();
  } else {
    stopCircuitPoll();
  }
}

function startCircuitPoll() {
  if (circuitPollTimer) return;
  circuitPollTimer = setInterval(loadCircuitStatus, 10_000);
}

function stopCircuitPoll() {
  if (circuitPollTimer) {
    clearInterval(circuitPollTimer);
    circuitPollTimer = null;
  }
}

function getCircuitStatuses(serverId: string): CircuitStatus[] {
  return circuitStatuses.value.filter((c) => c.server_id === serverId);
}

function formatCircuitModel(model: string): string {
  return model === '_any' ? 'all models' : model;
}

function formatCircuitRemaining(seconds: number): string {
  const m = Math.floor(seconds / 60);
  const s = seconds % 60;
  return m > 0 ? `${m}m ${s}s` : `${s}s`;
}

async function loadUptime() {
  if (!group.value) return;
  uptimeLoading.value = true;
  uptimeError.value = '';
  try {
    const { data: resp } = await api.get<{ servers: ServerUptime[] }>(`/api/admin/groups/${group.value.id}/uptime`);
    const map: Record<string, Bucket[]> = {};
    for (const s of resp.servers) {
      map[s.server_id] = s.buckets;
    }
    uptimeData.value = map;
  } catch {
    uptimeError.value = 'Unable to load status';
  } finally {
    uptimeLoading.value = false;
  }
}

function computeLocalRange(period: string): { start: string; end: string } | null {
  const now = new Date();
  if (period === 'today') {
    const start = new Date(now); start.setHours(0, 0, 0, 0);
    const end = new Date(now); end.setHours(23, 59, 59, 999);
    return { start: start.toISOString(), end: end.toISOString() };
  }
  if (period === 'yesterday') {
    const start = new Date(now); start.setDate(start.getDate() - 1); start.setHours(0, 0, 0, 0);
    const end = new Date(start); end.setHours(23, 59, 59, 999);
    return { start: start.toISOString(), end: end.toISOString() };
  }
  if (period === 'week') {
    const start = new Date(now);
    const dow = (start.getDay() + 6) % 7; // Monday=0
    start.setDate(start.getDate() - dow);
    start.setHours(0, 0, 0, 0);
    const end = new Date(start);
    end.setDate(end.getDate() + 6);
    end.setHours(23, 59, 59, 999);
    return { start: start.toISOString(), end: end.toISOString() };
  }
  if (period === 'month') {
    const start = new Date(now.getFullYear(), now.getMonth(), 1, 0, 0, 0, 0);
    const end = new Date(now.getFullYear(), now.getMonth() + 1, 0, 23, 59, 59, 999);
    return { start: start.toISOString(), end: end.toISOString() };
  }
  return null;
}

async function loadTokenUsage() {
  if (!group.value) return;
  tokenUsageLoading.value = true;
  tokenUsageError.value = '';
  try {
    const params: { period?: string; start?: string; end?: string; is_dynamic_key?: boolean; key_hash?: string } = {};
    const range = computeLocalRange(tokenUsagePeriod.value);
    if (range) {
      params.start = range.start;
      params.end = range.end;
    } else {
      params.period = tokenUsagePeriod.value;
    }
    if (tokenUsageDynamicKeyFilter.value) params.is_dynamic_key = true;
    if (tokenUsageKeyHashFilter.value?.trim()) params.key_hash = tokenUsageKeyHashFilter.value.trim();
    tokenUsageStats.value = await groupsStore.fetchTokenUsageStats(group.value.id, params);
  } catch {
    tokenUsageError.value = 'Failed to load token usage data';
  } finally {
    tokenUsageLoading.value = false;
  }
}

watch(tokenUsagePeriod, () => loadTokenUsage());
watch(tokenUsageDynamicKeyFilter, () => loadTokenUsage());
watch(tokenUsageKeyHashFilter, () => loadTokenUsage());
watch([subKeyDateStart, subKeyDateEnd], () => loadSubKeyUsage());

async function loadSubKeyUsage() {
  if (!group.value) return;
  subKeyUsageLoading.value = true;
  subKeyUsageError.value = '';
  try {
    const start = new Date(subKeyDateStart.value).toISOString();
    const end = new Date(`${subKeyDateEnd.value}T23:59:59.999Z`).toISOString();
    const result = await groupsStore.fetchTokenUsageByKey(group.value.id, { start, end });
    subKeyUsageData.value = result.keys;
  } catch {
    subKeyUsageError.value = 'Failed to load sub-key usage data';
  } finally {
    subKeyUsageLoading.value = false;
  }
}

async function loadSpam() {
  if (!group.value) return;
  spamLoading.value = true;
  spamError.value = '';
  try {
    const result = await groupsStore.fetchSpamDetection(group.value.id, {
      page: spamPagination.value.page,
      limit: spamPagination.value.rowsPerPage,
    });
    spamRows.value = result.data;
    spamPagination.value.rowsNumber = result.total;
  } catch {
    spamError.value = 'Failed to load spam detection data';
  } finally {
    spamLoading.value = false;
  }
}

function onSpamRequest(props: { pagination: { page: number; rowsPerPage: number } }) {
  spamPagination.value.page = props.pagination.page;
  spamPagination.value.rowsPerPage = props.pagination.rowsPerPage;
  loadSpam();
}

const filteredUaOptions = computed(() => {
  const text = uaFilterText.value.toLowerCase();
  return userAgents.value
    .map((ua) => ua.user_agent)
    .filter((ua) => !text || ua.toLowerCase().includes(text));
});

function onUaFilter(val: string, update: (fn: () => void) => void) {
  update(() => {
    uaFilterText.value = val;
  });
}

function onUaNewValue(val: string, done: (val: string) => void) {
  if (val.trim()) done(val.trim());
}

async function loadUserAgents() {
  if (!group.value) return;
  uaLoading.value = true;
  try {
    const [recorded, blocked] = await Promise.all([
      groupsStore.fetchGroupUserAgents(group.value.id),
      groupsStore.fetchGroupBlockedUserAgents(group.value.id),
    ]);
    userAgents.value = recorded;
    blockedUserAgents.value = blocked;
  } catch {
    $q.notify({ type: 'negative', message: 'Failed to load user agents' });
  } finally {
    uaLoading.value = false;
  }
}

async function onAddBlockedUa() {
  if (!group.value || !selectedUa.value) return;
  addingBlockedUa.value = true;
  try {
    await groupsStore.addGroupBlockedUserAgent(group.value.id, selectedUa.value);
    selectedUa.value = null;
    $q.notify({ type: 'positive', message: 'User agent blocked' });
    await loadUserAgents();
  } catch {
    $q.notify({ type: 'negative', message: 'Failed to block user agent' });
  } finally {
    addingBlockedUa.value = false;
  }
}

async function onRemoveBlockedUa(ua: string) {
  if (!group.value) return;
  removingBlockedUa.value = ua;
  try {
    await groupsStore.removeGroupBlockedUserAgent(group.value.id, ua);
    $q.notify({ type: 'positive', message: 'User agent unblocked' });
    await loadUserAgents();
  } catch {
    $q.notify({ type: 'negative', message: 'Failed to unblock user agent' });
  } finally {
    removingBlockedUa.value = null;
  }
}

// Sub-key methods
function maskKey(key: string) {
  if (key.length <= 16) return key;
  return `${key.slice(0, 14)}...${key.slice(-4)}`;
}

async function loadSubKeys() {
  if (!group.value) return;
  subKeysLoading.value = true;
  subKeyError.value = '';
  try {
    const params: { page?: number; limit?: number; search?: string } = {
      page: subKeyPagination.value.page,
      limit: subKeyPagination.value.rowsPerPage,
    };
    if (subKeySearch.value) params.search = subKeySearch.value;
    const result = await groupsStore.fetchGroupKeys(group.value.id, params);
    subKeys.value = result.data;
    subKeyPagination.value.rowsNumber = result.total;
  } catch {
    subKeyError.value = 'Failed to load sub-keys';
  } finally {
    subKeysLoading.value = false;
  }
}

function onSubKeyRequest(props: { pagination: { page: number; rowsPerPage: number } }) {
  subKeyPagination.value.page = props.pagination.page;
  subKeyPagination.value.rowsPerPage = props.pagination.rowsPerPage;
  loadSubKeys();
}

async function onCreateKey() {
  if (!group.value || !newKeyName.value.trim()) return;
  creatingKey.value = true;
  try {
    await groupsStore.createGroupKey(group.value.id, newKeyName.value.trim());
    showCreateKey.value = false;
    newKeyName.value = '';
    loadSubKeys();
    $q.notify({ type: 'positive', message: 'Key created' });
  } catch (e: unknown) {
    const msg = (e as { response?: { data?: { error?: string } } })?.response?.data?.error || 'Failed to create key';
    $q.notify({ type: 'negative', message: msg });
  } finally {
    creatingKey.value = false;
  }
}

function openBulkCreate() {
  showBulkCreate.value = true;
  bulkCreateForm.value.custom_expires_at = '';
  bulkCreateForm.value.custom_expires_model = '';
  loadActivePlans();
}

function onBulkCreateDatePick(val: string) {
  bulkCreateForm.value.custom_expires_model = val;
  bulkCreateForm.value.custom_expires_at = val.replace(/\//g, '-');
}

function clearBulkCreateExpiry() {
  bulkCreateForm.value.custom_expires_at = '';
  bulkCreateForm.value.custom_expires_model = '';
}

async function onBulkCreate() {
  if (!group.value || !bulkCreateForm.value.plan_id || bulkCreateForm.value.count < 1) return;
  bulkCreating.value = true;
  try {
    const payload: { count: number; plan_id: string; name_prefix?: string; custom_expires_at?: string } = {
      count: bulkCreateForm.value.count,
      plan_id: bulkCreateForm.value.plan_id,
    };
    if (bulkCreateForm.value.name_prefix.trim()) {
      payload.name_prefix = bulkCreateForm.value.name_prefix.trim();
    }
    if (bulkCreateForm.value.custom_expires_at) {
      payload.custom_expires_at = bulkCreateForm.value.custom_expires_at;
    }
    bulkCreatedKeys.value = await groupsStore.bulkCreateGroupKeys(group.value.id, payload);
    showBulkCreate.value = false;
    showBulkResult.value = true;
    loadSubKeys();
  } catch (e: unknown) {
    const msg = (e as { response?: { data?: { error?: string } } })?.response?.data?.error || 'Failed to bulk create keys';
    $q.notify({ type: 'negative', message: msg });
  } finally {
    bulkCreating.value = false;
  }
}

function downloadBulkCsv() {
  const baseUrl = window.location.origin;
  const rows = [
    'api_key,base_url,dashboard_url',
    ...bulkCreatedKeys.value.map((k) => {
      const dashboardUrl = `${baseUrl}/#/usage/${encodeURIComponent(k.api_key)}`;
      return `${k.api_key},${baseUrl},${dashboardUrl}`;
    }),
  ];
  const blob = new Blob([rows.join('\n')], { type: 'text/csv' });
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = 'sub-keys.csv';
  a.click();
  URL.revokeObjectURL(url);
}

async function toggleSubKey(row: GroupKey, val: boolean) {
  if (!group.value) return;
  try {
    const updated = await groupsStore.updateGroupKey(group.value.id, row.id, { is_active: val });
    Object.assign(row, updated);
  } catch {
    $q.notify({ type: 'negative', message: 'Failed to update key status' });
  }
}

async function onRegenerateSubKey(row: GroupKey) {
  if (!group.value) return;
  $q.dialog({ title: 'Regenerate Key', message: `Regenerate key "${row.name}"? The old key will stop working.`, cancel: true })
    .onOk(async () => {
      if (!group.value) return;
      try {
        const updated = await groupsStore.regenerateGroupKey(group.value.id, row.id);
        Object.assign(row, updated);
        $q.notify({ type: 'positive', message: 'Key regenerated' });
      } catch {
        $q.notify({ type: 'negative', message: 'Failed to regenerate key' });
      }
    });
}

async function onDeleteSubKey(row: GroupKey) {
  if (!group.value) return;
  $q.dialog({ title: 'Delete Key', message: `Delete key "${row.name}"? This action cannot be undone.`, cancel: true })
    .onOk(async () => {
      if (!group.value) return;
      try {
        await groupsStore.deleteGroupKey(group.value.id, row.id);
        loadSubKeys();
        $q.notify({ type: 'positive', message: 'Key deleted' });
      } catch {
        $q.notify({ type: 'negative', message: 'Failed to delete key' });
      }
    });
}

watch(activeTab, (tab) => {
  router.replace({ query: { ...route.query, tab } });
  if (tab === 'keys' && subKeys.value.length === 0) loadSubKeys();
  if (tab === 'allowed-models') loadAllowedModels();
  if (tab === 'token-usage') {
    loadTokenUsage();
    if (tokenUsageChildTab.value === 'by-sub-key' && subKeyUsageData.value.length === 0) {
      loadSubKeyUsage();
    }
  }
  if (tab === 'spam') loadSpam();
  if (tab === 'user-agents') loadUserAgents();
});

watch(tokenUsageChildTab, (childTab) => {
  if (childTab === 'by-sub-key' && subKeyUsageData.value.length === 0) {
    loadSubKeyUsage();
  }
});

function onCbFieldClear(field: 'cb_max_failures' | 'cb_window_seconds' | 'cb_cooldown_seconds') {
  if (editServerCbForm.value[field] === null || editServerCbForm.value[field] === undefined) {
    editServerCbForm.value.cb_max_failures = null;
    editServerCbForm.value.cb_window_seconds = null;
    editServerCbForm.value.cb_cooldown_seconds = null;
  }
}

function onRlFieldClear() {
  editServerRlForm.value.max_requests = null;
  editServerRlForm.value.rate_window_seconds = null;
}

function parseDurationStr(s: string): number | null {
  const m = /^(\d+)\s*(s|m|h|d|w)$/i.exec(s.trim());
  if (!m || !m[1] || !m[2]) return null;
  const mult: Record<string, number> = { s: 1, m: 60, h: 3600, d: 86400, w: 604800 };
  const unit = mult[m[2].toLowerCase()];
  if (unit === undefined) return null;
  return parseInt(m[1], 10) * unit;
}

function formatDurationSecs(secs: number): string {
  const units: [number, string][] = [[604800, 'w'], [86400, 'd'], [3600, 'h'], [60, 'm'], [1, 's']];
  for (const [n, u] of units) {
    if (secs % n === 0) return `${secs / n}${u}`;
  }
  return `${secs}s`;
}

function onPkRlFieldClear() {
  editServerPkRlForm.value.per_key_max_requests = null;
  editServerPkRlForm.value.per_key_window_str = '';
  editServerPkRlForm.value.per_key_window_error = '';
}

async function saveGroup() {
  if (!group.value) return;
  const codes = failoverCodesStr.value
    .split(',')
    .map((s) => parseInt(s.trim(), 10))
    .filter((n) => !Number.isNaN(n));
  const ttftVal = ttftTimeoutStr.value.trim();
  const ttft_timeout_ms = ttftVal === '' ? null : parseInt(ttftVal, 10);
  await groupsStore.updateGroup(group.value.id, {
    name: group.value.name,
    failover_status_codes: codes,
    is_active: group.value.is_active,
    ttft_timeout_ms: Number.isNaN(ttft_timeout_ms as number) ? null : ttft_timeout_ms,
    count_tokens_server_id: group.value.count_tokens_server_id,
  });
  $q.notify({ type: 'positive', message: 'Saved' });
}

async function saveCtMappings() {
  if (!group.value) return;
  const mappings: Record<string, string> = {};
  for (const e of ctMappingEntries.value) {
    if (e.from && e.to) mappings[e.from] = e.to;
  }
  await groupsStore.updateGroup(group.value.id, {
    count_tokens_model_mappings: mappings,
  });
  group.value.count_tokens_model_mappings = mappings;
  $q.notify({ type: 'positive', message: 'Count tokens mappings saved' });
}

function copyKey() {
  if (group.value) {
    copyToClipboard(group.value.api_key).then(() =>
      $q.notify({ message: 'Copied', type: 'positive' })
    );
  }
}

function copyText(text: string) {
  copyToClipboard(text).then(() =>
    $q.notify({ message: 'Copied', type: 'positive' })
  );
}

function copyShortId(shortId: number) {
  copyToClipboard(String(shortId)).then(() =>
    $q.notify({ message: 'Copied', type: 'positive' })
  );
}

function usageLink(apiKey: string) {
  return `${window.location.origin}/#/usage/${encodeURIComponent(apiKey)}`;
}

async function onRegenerate() {
  if (!group.value) return;
  $q.dialog({ title: 'Regenerate Key', message: 'This will invalidate the current key.', cancel: true })
    .onOk(async () => {
      if (!group.value) return;
      const updated = await groupsStore.regenerateKey(group.value.id);
      group.value.api_key = updated.api_key;
      $q.notify({ type: 'positive', message: 'Key regenerated' });
    });
}

function resetAddForm() {
  addForm.value = { server_id: '', priority: 1 };
  newServerForm.value = { name: '', base_url: '', api_key: '' };
}

async function onAddServer() {
  if (!group.value) return;
  addingServer.value = true;
  try {
    let serverId = addForm.value.server_id;
    if (isCreatingNew.value) {
      const input: { name: string; base_url: string; api_key?: string } = {
        name: newServerForm.value.name,
        base_url: newServerForm.value.base_url,
      };
      if (newServerForm.value.api_key) input.api_key = newServerForm.value.api_key;
      const created = await serversStore.createServer(input);
      serverId = created.id;
      // refresh server list for future use
      const sData = await serversStore.fetchServers({ limit: 100 });
      if (sData) {
        allServers.value = sData.data.map((s) => ({ label: `${s.name} (#${s.short_id})`, value: s.id }));
      }
    }
    await groupsStore.assignServer(group.value.id, { server_id: serverId, priority: addForm.value.priority });
    showAddServer.value = false;
    loadGroup();
  } catch (e: unknown) {
    const msg = (e as { response?: { data?: { error?: string } } })?.response?.data?.error || 'Failed to add server';
    $q.notify({ type: 'negative', message: msg });
  } finally {
    addingServer.value = false;
  }
}

async function onRemoveServer(s: GroupServerDetail) {
  if (!group.value) return;
  $q.dialog({ title: 'Remove Server', message: `Remove "${s.server_name}"?`, cancel: true })
    .onOk(async () => {
      if (!group.value) return;
      await groupsStore.removeServer(group.value.id, s.server_id);
      loadGroup();
    });
}

async function moveServer(idx: number, direction: number) {
  if (!group.value) return;
  const arr = [...servers.value];
  const target = idx + direction;
  const a = arr[idx];
  const b = arr[target];
  if (!a || !b) return;
  arr[idx] = b;
  arr[target] = a;
  await groupsStore.reorderServers(group.value.id, arr.map((s) => s.server_id));
  loadGroup();
}

async function toggleServerEnabled(s: GroupServerDetail) {
  if (!group.value) return;
  try {
    await groupsStore.updateAssignment(group.value.id, s.server_id, { is_enabled: s.is_enabled });
    await loadGroup();
  } catch {
    s.is_enabled = !s.is_enabled;
    $q.notify({ type: 'negative', message: 'Failed to update server status' });
  }
}

const removingSupportedModel = ref('');

async function onRemoveSupportedModel(s: GroupServerDetail, model: string) {
  if (!group.value) return;
  removingSupportedModel.value = `${s.server_id}:${model}`;
  try {
    const updated = (s.supported_models || []).filter((m) => m !== model);
    await groupsStore.updateAssignment(group.value.id, s.server_id, { supported_models: updated });
    await loadGroup();
  } catch {
    $q.notify({ type: 'negative', message: `Failed to remove model "${model}"` });
  } finally {
    removingSupportedModel.value = '';
  }
}

function editMappings(s: GroupServerDetail) {
  editingMapping.value = s;
  const m = s.model_mappings || {};
  mappingEntries.value = Object.entries(m).map(([from, to]) => ({ from, to }));
  showMappings.value = true;
}

async function saveMappings() {
  if (!group.value || !editingMapping.value) return;
  const mappings: Record<string, string> = {};
  for (const e of mappingEntries.value) {
    if (e.from && e.to) mappings[e.from] = e.to;
  }
  await groupsStore.updateAssignment(group.value.id, editingMapping.value.server_id, { model_mappings: mappings });
  showMappings.value = false;
  loadGroup();
}

async function unlockDialog(serverId: string): Promise<void> {
  if (await serversStore.tryUnlockFromCache(serverId)) {
    await loadGroup();
    $q.notify({ type: 'positive', message: 'Server unlocked' });
    return;
  }
  await new Promise<void>((resolve) => {
    $q.dialog({
      title: 'Unlock Server',
      message: 'Enter the server password:',
      prompt: { model: '', type: 'password' as const },
      cancel: true,
    }).onOk(async (pw: string) => {
      try {
        await serversStore.unlockServer(serverId, pw);
        // Re-fetch so API returns real credentials (backend session is now unlocked)
        await loadGroup();
        $q.notify({ type: 'positive', message: 'Server unlocked' });
        resolve();
      } catch (e: unknown) {
        const msg = (e as Error).message || 'Failed to unlock';
        $q.notify({ type: 'negative', message: msg });
        resolve();
      }
    }).onCancel(() => resolve());
  });
}

async function openEditServer(s: GroupServerDetail) {
  if (serversStore.isProtected(s.server_id) && !serversStore.isUnlocked(s.server_id)) {
    if (await serversStore.tryUnlockFromCache(s.server_id)) {
      await loadGroup();
    } else {
      let cancelled = true;
      await new Promise<void>((resolve) => {
        $q.dialog({
          title: 'Unlock Server',
          message: 'Enter the server password to edit:',
          prompt: { model: '', type: 'password' as const },
          cancel: true,
        }).onOk(async (pw: string) => {
          try {
            await serversStore.unlockServer(s.server_id, pw);
            // Re-fetch so API returns real credentials
            await loadGroup();
            cancelled = false;
            resolve();
          } catch (e: unknown) {
            const msg = (e as Error).message || 'Failed to unlock';
            $q.notify({ type: 'negative', message: msg });
            resolve();
          }
        }).onCancel(() => resolve());
      });
      if (cancelled) return;
    }
    // Get fresh server data with real credentials
    const fresh = servers.value.find((x) => x.server_id === s.server_id);
    if (!fresh) return;
    doOpenEditServer(fresh);
    return;
  }

  doOpenEditServer(s);
}

function doOpenEditServer(s: GroupServerDetail) {
  editServerId.value = s.server_id;
  const fullServer = serversStore.servers.find((srv) => srv.id === s.server_id);
  editServerForm.value = { name: s.server_name, base_url: s.base_url || '', api_key: s.api_key || '', system_prompt: fullServer?.system_prompt || '', remove_thinking: fullServer?.remove_thinking ?? false };
  const ch = fullServer?.custom_headers || s.custom_headers;
  editServerCustomHeaders.value = ch ? Object.entries(ch).map(([name, value]) => ({ name, value })) : [];
  editServerCbForm.value = {
    cb_max_failures: s.cb_max_failures,
    cb_window_seconds: s.cb_window_seconds,
    cb_cooldown_seconds: s.cb_cooldown_seconds,
  };
  editServerRlForm.value = {
    max_requests: s.max_requests,
    rate_window_seconds: s.rate_window_seconds,
  };
  editServerPkRlForm.value = {
    per_key_max_requests: s.per_key_max_requests,
    per_key_window_str: s.per_key_rate_window_seconds != null ? formatDurationSecs(s.per_key_rate_window_seconds) : '',
    per_key_window_error: '',
  };
  editServerTokenForm.value = {
    max_input_tokens: s.max_input_tokens,
    min_input_tokens: s.min_input_tokens,
  };
  editServerSupportedModels.value = s.supported_models ? [...s.supported_models] : [];
  editServerActiveHoursForm.value = {
    timezone: s.active_hours_timezone ?? null,
    start: s.active_hours_start ?? '',
    end: s.active_hours_end ?? '',
  };
  filteredTimezoneOptions.value = IANA_TIMEZONE_LIST;
  showEditServer.value = true;
}

function onEditServerModelFilter(val: string, update: (fn: () => void) => void) {
  const params: { limit: number; search?: string } = { limit: 100 };
  if (val) params.search = val;
  modelsStore.fetchModels(params).then((result) => {
    update(() => {
      editServerModelOptions.value = result.data.map((m) => m.name);
    });
  }).catch(() => {
    update(() => {
      editServerModelOptions.value = [];
    });
  });
}

async function onSaveEditServer() {
  // Validate CB fields: all-or-nothing
  const { cb_max_failures, cb_window_seconds, cb_cooldown_seconds } = editServerCbForm.value;
  const cbValues = [cb_max_failures, cb_window_seconds, cb_cooldown_seconds];
  const cbNonNull = cbValues.filter((v) => v !== null && v !== undefined);
  if (cbNonNull.length > 0 && cbNonNull.length < 3) {
    $q.notify({ type: 'negative', message: 'Circuit breaker requires all three fields or none.' });
    return;
  }
  if (cbNonNull.length === 3 && cbNonNull.some((v) => (v as number) < 1)) {
    $q.notify({ type: 'negative', message: 'Circuit breaker values must be >= 1.' });
    return;
  }

  // Validate rate limit fields: all-or-nothing
  const { max_requests, rate_window_seconds } = editServerRlForm.value;
  const rlValues = [max_requests, rate_window_seconds];
  const rlNonNull = rlValues.filter((v) => v !== null && v !== undefined);
  if (rlNonNull.length === 1) {
    $q.notify({ type: 'negative', message: 'Rate limit requires both fields or none.' });
    return;
  }
  if (rlNonNull.length === 2 && rlNonNull.some((v) => (v as number) < 1)) {
    $q.notify({ type: 'negative', message: 'Rate limit values must be >= 1.' });
    return;
  }

  // Validate per-key rate limit fields: all-or-nothing
  const pkMax = editServerPkRlForm.value.per_key_max_requests;
  const pkWindowStr = editServerPkRlForm.value.per_key_window_str.trim();
  let pkWindowSecs: number | null = null;
  editServerPkRlForm.value.per_key_window_error = '';
  const pkMaxSet = pkMax !== null && pkMax !== undefined;
  const pkWindowSet = pkWindowStr !== '';
  if (pkMaxSet !== pkWindowSet) {
    $q.notify({ type: 'negative', message: 'Per-key rate limit requires both fields or none.' });
    return;
  }
  if (pkMaxSet && pkWindowSet) {
    if ((pkMax as number) < 1) {
      $q.notify({ type: 'negative', message: 'Per-key max requests must be >= 1.' });
      return;
    }
    pkWindowSecs = parseDurationStr(pkWindowStr);
    if (pkWindowSecs === null || pkWindowSecs < 1) {
      editServerPkRlForm.value.per_key_window_error = 'Use a value like 1s, 1m, 1h, 1d, 1w';
      $q.notify({ type: 'negative', message: 'Per-key window is invalid. Use 1s/1m/1h/1d/1w.' });
      return;
    }
  }

  savingServer.value = true;
  try {
    // Convert custom headers array to object, filtering empty entries
    const headersObj: Record<string, string> = {};
    for (const e of editServerCustomHeaders.value) {
      if (e.name && e.value) headersObj[e.name] = e.value;
    }
    const customHeadersPayload = Object.keys(headersObj).length > 0 ? headersObj : null;

    await serversStore.updateServer(editServerId.value, {
      name: editServerForm.value.name,
      base_url: editServerForm.value.base_url,
      api_key: editServerForm.value.api_key || null,
      system_prompt: editServerForm.value.system_prompt || null,
      remove_thinking: editServerForm.value.remove_thinking,
      custom_headers: customHeadersPayload,
    });
    // Save circuit breaker fields via assignment update
    if (group.value) {
      const ahTimezone = editServerActiveHoursForm.value.timezone || null;
      const ahStart = editServerActiveHoursForm.value.start || null;
      const ahEnd = editServerActiveHoursForm.value.end || null;
      await groupsStore.updateAssignment(group.value.id, editServerId.value, {
        cb_max_failures: editServerCbForm.value.cb_max_failures,
        cb_window_seconds: editServerCbForm.value.cb_window_seconds,
        cb_cooldown_seconds: editServerCbForm.value.cb_cooldown_seconds,
        max_requests: editServerRlForm.value.max_requests,
        rate_window_seconds: editServerRlForm.value.rate_window_seconds,
        per_key_max_requests: pkMaxSet ? (pkMax as number) : null,
        per_key_rate_window_seconds: pkMaxSet ? pkWindowSecs : null,
        max_input_tokens: editServerTokenForm.value.max_input_tokens,
        min_input_tokens: editServerTokenForm.value.min_input_tokens,
        supported_models: editServerSupportedModels.value,
        active_hours_start: ahStart,
        active_hours_end: ahEnd,
        active_hours_timezone: ahTimezone,
      });
    }
    showEditServer.value = false;
    loadGroup();
    loadCircuitStatus();
    // refresh server list for add dialog
    const sData = await serversStore.fetchServers({ limit: 100 });
    if (sData) {
      allServers.value = sData.data.map((s) => ({ label: `${s.name} (#${s.short_id})`, value: s.id }));
    }
  } catch (e: unknown) {
    const msg = (e as { response?: { data?: { error?: string } } })?.response?.data?.error || 'Failed to save server';
    $q.notify({ type: 'negative', message: msg });
  } finally {
    savingServer.value = false;
  }
}

// Allowed models methods
async function loadAllowedModels() {
  if (!group.value) return;
  allowedModels.value = group.value.allowed_models || [];
  // Load master models list for the picker
  try {
    const result = await modelsStore.fetchModels({ limit: 100 });
    allModelsOptions.value = result.data
      .filter((m) => !allowedModels.value.some((am) => am.id === m.id))
      .map((m) => ({ label: m.name, value: m.id }));
    filteredModelOptions.value = allModelsOptions.value;
  } catch {
    // silently fail
  }
}

function onModelFilter(val: string, update: (fn: () => void) => void) {
  modelSearchText.value = val;
  update(() => {
    const needle = val.toLowerCase();
    filteredModelOptions.value = allModelsOptions.value.filter((o) => o.label.toLowerCase().includes(needle));
  });
}

function onNewModelValue(val: string, done: (item?: { label: string; value: string }, mode?: 'add' | 'add-unique' | 'toggle') => void) {
  // When user types a new value and presses Enter, create the model inline
  if (val.trim()) {
    onCreateAndAddModel(val.trim());
  }
  done();
}

async function onAddAllowedModel(modelId: string | null) {
  if (!group.value || !modelId) return;
  allowedModelsLoading.value = true;
  try {
    await groupsStore.addGroupAllowedModel(group.value.id, { model_id: modelId });
    modelToAdd.value = null;
    await loadGroup();
    loadAllowedModels();
    $q.notify({ type: 'positive', message: 'Model added' });
  } catch (e: unknown) {
    const msg = (e as { response?: { data?: { error?: string } } })?.response?.data?.error || 'Failed to add model';
    $q.notify({ type: 'negative', message: msg });
  } finally {
    allowedModelsLoading.value = false;
  }
}

async function onCreateAndAddModel(name?: string) {
  const modelName = (name || modelSearchText.value).trim();
  if (!group.value || !modelName) return;
  allowedModelsLoading.value = true;
  try {
    await groupsStore.addGroupAllowedModel(group.value.id, { name: modelName });
    modelToAdd.value = null;
    modelSearchText.value = '';
    await loadGroup();
    loadAllowedModels();
    $q.notify({ type: 'positive', message: `Model "${modelName}" created and added` });
  } catch (e: unknown) {
    const msg = (e as { response?: { data?: { error?: string } } })?.response?.data?.error || 'Failed to create model';
    $q.notify({ type: 'negative', message: msg });
  } finally {
    allowedModelsLoading.value = false;
  }
}

async function onRemoveAllowedModel(m: Model) {
  if (!group.value) return;
  allowedModelsLoading.value = true;
  try {
    await groupsStore.removeGroupAllowedModel(group.value.id, m.id);
    await loadGroup();
    loadAllowedModels();
    $q.notify({ type: 'positive', message: `"${m.name}" removed` });
  } catch (e: unknown) {
    const msg = (e as { response?: { data?: { error?: string } } })?.response?.data?.error || 'Failed to remove model';
    $q.notify({ type: 'negative', message: msg });
  } finally {
    allowedModelsLoading.value = false;
  }
}

// Key allowed models methods
function onExpandSubKey(props: { expand: boolean; row: GroupKey }) {
  props.expand = !props.expand;
  if (props.expand) {
    if (allowedModels.value.length > 0) {
      loadKeyAllowedModels(props.row.id);
    }
    if (servers.value.length > 0) {
      loadKeyServers(props.row.id);
    }
    loadKeySubscriptions(props.row.id);
    loadActivePlans();
  }
}

async function loadKeySubscriptions(keyId: string, page?: number, limit?: number) {
  if (!group.value) return;
  const pg = subPagination.value[keyId] || { page: 1, rowsPerPage: 10, rowsNumber: 0 };
  const p = page ?? pg.page;
  const l = limit ?? pg.rowsPerPage;
  keySubsLoading.value[keyId] = true;
  try {
    const { data } = await api.get<{ data: KeySubscription[]; total: number; page: number; total_pages: number }>(`/api/admin/groups/${group.value.id}/keys/${keyId}/subscriptions`, { params: { page: p, limit: l } });
    keySubscriptions.value[keyId] = { data: data.data, total: data.total };
    subPagination.value[keyId] = { page: data.page, rowsPerPage: l, rowsNumber: data.total };
  } catch { /* ignore */ } finally {
    keySubsLoading.value[keyId] = false;
  }
}

function onSubPaginationRequest(keyId: string, req: { pagination: { page: number; rowsPerPage: number } }) {
  loadKeySubscriptions(keyId, req.pagination.page, req.pagination.rowsPerPage);
}

async function loadActivePlans() {
  if (activePlans.value.length > 0) return;
  try {
    const { data } = await api.get<SubscriptionPlan[]>('/api/admin/subscription-plans');
    activePlans.value = data.filter((p) => p.is_active);
  } catch { /* ignore */ }
}

function onOpenAssignSub(keyId: string) {
  assignSubKeyId.value = keyId;
  assignSubPlanId.value = '';
  assignSubExpiryDate.value = '';
  assignSubExpiryModel.value = '';
  showAssignSubDialog.value = true;
  loadActivePlans();
}

function clearAssignSubExpiry() {
  assignSubExpiryDate.value = '';
  assignSubExpiryModel.value = '';
}

function onAssignSubDatePick(val: string) {
  assignSubExpiryModel.value = val;
  assignSubExpiryDate.value = val.replace(/\//g, '-');
}

async function onAssignSubscription(keyId: string, planId: string, expiryDate?: string) {
  if (!group.value || !planId) return;
  try {
    const payload: { plan_id: string; custom_expires_at?: string } = { plan_id: planId };
    if (expiryDate) {
      payload.custom_expires_at = expiryDate;
    }
    await api.post(`/api/admin/groups/${group.value.id}/keys/${keyId}/subscriptions`, payload);
    showAssignSubDialog.value = false;
    loadKeySubscriptions(keyId);
    $q.notify({ type: 'positive', message: 'Subscription assigned' });
  } catch (e: unknown) {
    const msg = (e as { response?: { data?: { error?: string } } })?.response?.data?.error || 'Failed to assign';
    $q.notify({ type: 'negative', message: msg });
  }
}

function onOpenAddBonus(keyId: string) {
  addBonusKeyId.value = keyId;
  addBonusForm.value = {
    bonus_name: '',
    bonus_base_url: 'https://api.anthropic.com',
    bonus_api_key: '',
    bonus_quota_url: '',
    bonus_allowed_models: [],
  };
  addBonusQuotaHeaders.value = [];
  addBonusCustomHeaders.value = [];
  showAddBonusDialog.value = true;
}

function headersToRecord(rows: { name: string; value: string }[]): Record<string, string> | null {
  const obj: Record<string, string> = {};
  for (const e of rows) {
    if (e.name.trim() && e.value !== '') obj[e.name.trim()] = e.value;
  }
  return Object.keys(obj).length > 0 ? obj : null;
}

async function onSubmitAddBonus() {
  if (!group.value) return;
  if (!addBonusForm.value.bonus_name.trim()) {
    $q.notify({ type: 'negative', message: 'Name is required' });
    return;
  }
  if (!addBonusForm.value.bonus_base_url.trim()) {
    $q.notify({ type: 'negative', message: 'Base URL is required' });
    return;
  }
  if (!addBonusForm.value.bonus_api_key.trim()) {
    $q.notify({ type: 'negative', message: 'API Key is required' });
    return;
  }

  const bonus_quota_headers = headersToRecord(addBonusQuotaHeaders.value);
  const bonus_custom_headers = headersToRecord(addBonusCustomHeaders.value);

  addBonusLoading.value = true;
  try {
    await api.post(`/api/admin/groups/${group.value.id}/keys/${addBonusKeyId.value}/subscriptions`, {
      bonus_name: addBonusForm.value.bonus_name.trim(),
      bonus_base_url: addBonusForm.value.bonus_base_url.trim(),
      bonus_api_key: addBonusForm.value.bonus_api_key.trim(),
      bonus_quota_url: addBonusForm.value.bonus_quota_url.trim() || null,
      bonus_quota_headers,
      bonus_allowed_models: addBonusForm.value.bonus_allowed_models,
      bonus_custom_headers,
    });
    showAddBonusDialog.value = false;
    addBonusForm.value.bonus_allowed_models = [];
    loadKeySubscriptions(addBonusKeyId.value);
    $q.notify({ type: 'positive', message: 'Bonus subscription added' });
  } catch (e: unknown) {
    const msg = (e as { response?: { data?: { error?: string } } })?.response?.data?.error || 'Failed to add bonus';
    $q.notify({ type: 'negative', message: msg });
  } finally {
    addBonusLoading.value = false;
  }
}

function onOpenEditBonusHeaders(keyId: string, sub: KeySubscription) {
  if (!keyId || sub.sub_type !== 'bonus') return;
  editBonusHeadersKeyId.value = keyId;
  editBonusHeadersSubId.value = sub.id;
  editBonusHeadersForm.value = {
    bonus_quota_url: sub.bonus_quota_url ?? '',
    bonus_non_stream_timeout_ms: sub.bonus_non_stream_timeout_ms ?? null,
  };
  editBonusQuotaHeaders.value = sub.bonus_quota_headers
    ? Object.entries(sub.bonus_quota_headers).map(([name, value]) => ({ name, value }))
    : [];
  editBonusCustomHeaders.value = sub.bonus_custom_headers
    ? Object.entries(sub.bonus_custom_headers).map(([name, value]) => ({ name, value }))
    : [];
  showEditBonusHeadersDialog.value = true;
}

async function onSaveBonusHeaders() {
  if (!group.value || !editBonusHeadersKeyId.value || !editBonusHeadersSubId.value) return;
  editBonusHeadersLoading.value = true;
  const groupId = group.value.id;
  const keyId = editBonusHeadersKeyId.value;
  const subId = editBonusHeadersSubId.value;
  try {
    await api.put(
      `/api/admin/groups/${groupId}/keys/${keyId}/subscriptions/${subId}/bonus-quota-config`,
      {
        bonus_quota_url: editBonusHeadersForm.value.bonus_quota_url.trim() || null,
        bonus_quota_headers: headersToRecord(editBonusQuotaHeaders.value),
      },
    );
    await api.put(
      `/api/admin/groups/${groupId}/keys/${keyId}/subscriptions/${subId}/bonus-custom-headers`,
      { bonus_custom_headers: headersToRecord(editBonusCustomHeaders.value) },
    );
    await api.put(
      `/api/admin/groups/${groupId}/keys/${keyId}/subscriptions/${subId}/bonus-non-stream-timeout`,
      { bonus_non_stream_timeout_ms: editBonusHeadersForm.value.bonus_non_stream_timeout_ms ?? null },
    );
    showEditBonusHeadersDialog.value = false;
    editBonusHeadersKeyId.value = '';
    editBonusHeadersSubId.value = '';
    loadKeySubscriptions(keyId);
    $q.notify({ type: 'positive', message: 'Bonus config updated' });
  } catch (e: unknown) {
    const msg = (e as { response?: { data?: { error?: string } } })?.response?.data?.error || 'Failed to update bonus config';
    $q.notify({ type: 'negative', message: msg });
  } finally {
    editBonusHeadersLoading.value = false;
  }
}

async function onCancelSubscription(keyId: string, subId: string) {
  if (!group.value) return;
  try {
    await api.patch(`/api/admin/groups/${group.value.id}/keys/${keyId}/subscriptions/${subId}`, { status: 'cancelled' });
    loadKeySubscriptions(keyId);
    $q.notify({ type: 'positive', message: 'Subscription cancelled' });
  } catch (e: unknown) {
    const msg = (e as { response?: { data?: { error?: string } } })?.response?.data?.error || 'Failed to cancel';
    $q.notify({ type: 'negative', message: msg });
  }
}

async function onMoveBonus(keyId: string, subId: string, direction: 'up' | 'down') {
  if (!group.value) return;
  const subs = keySubscriptions.value[keyId]?.data ?? [];
  const bonusSubs = subs.filter((s) => s.sub_type === 'bonus');
  const idx = bonusSubs.findIndex((s) => s.id === subId);
  if (idx < 0) return;
  const swapIdx = direction === 'up' ? idx - 1 : idx + 1;
  if (swapIdx < 0 || swapIdx >= bonusSubs.length) return;

  const ordered = bonusSubs.map((s) => s.id);
  const tmp = ordered[idx] as string;
  ordered[idx] = ordered[swapIdx] as string;
  ordered[swapIdx] = tmp;

  reorderingBonusSubs.value.add(subId);
  try {
    await api.put(
      `/api/admin/groups/${group.value.id}/keys/${keyId}/subscriptions/reorder`,
      { ordered_ids: ordered },
    );
    await loadKeySubscriptions(keyId);
  } catch (e: unknown) {
    const msg =
      (e as { response?: { data?: { error?: string } } })?.response?.data?.error ||
      'Failed to reorder';
    $q.notify({ type: 'negative', message: msg });
    await loadKeySubscriptions(keyId);
  } finally {
    reorderingBonusSubs.value.delete(subId);
  }
}

function getBonusIndex(keyId: string, subId: string): number {
  const subs = keySubscriptions.value[keyId]?.data ?? [];
  const bonusSubs = subs.filter((s) => s.sub_type === 'bonus');
  return bonusSubs.findIndex((s) => s.id === subId);
}

function getBonusCount(keyId: string): number {
  const subs = keySubscriptions.value[keyId]?.data ?? [];
  return subs.filter((s) => s.sub_type === 'bonus').length;
}

function canEditBonusAllowedModels(sub: KeySubscription): boolean {
  return sub.sub_type === 'bonus' && sub.status === 'active';
}

function onOpenEditBonusModels(keyId: string, sub: KeySubscription) {
  if (!keyId || !canEditBonusAllowedModels(sub)) return;
  editBonusModelsKeyId.value = keyId;
  editBonusModelsSubId.value = sub.id;
  editBonusModelsForm.value = {
    bonus_allowed_models: [...(sub.bonus_allowed_models ?? [])],
  };
  showEditBonusModelsDialog.value = true;
}

async function onSaveBonusAllowedModels() {
  if (!group.value || !editBonusModelsKeyId.value || !editBonusModelsSubId.value) return;
  editBonusModelsLoading.value = true;
  try {
    await api.put(
      `/api/admin/groups/${group.value.id}/keys/${editBonusModelsKeyId.value}/subscriptions/${editBonusModelsSubId.value}/bonus-allowed-models`,
      { bonus_allowed_models: editBonusModelsForm.value.bonus_allowed_models },
    );
    const keyId = editBonusModelsKeyId.value;
    showEditBonusModelsDialog.value = false;
    editBonusModelsKeyId.value = '';
    editBonusModelsSubId.value = '';
    loadKeySubscriptions(keyId);
    $q.notify({ type: 'positive', message: 'Allowed models updated' });
  } catch (e: unknown) {
    const msg = (e as { response?: { data?: { error?: string } } })?.response?.data?.error || 'Failed to update allowed models';
    $q.notify({ type: 'negative', message: msg });
  } finally {
    editBonusModelsLoading.value = false;
  }
}

function getKeyAllowedModels(keyId: string): Model[] {
  return keyAllowedModelsMap.value[keyId] || [];
}

function keyModelOptions(keyId: string) {
  const keyModels = getKeyAllowedModels(keyId);
  return allowedModels.value
    .filter((m) => !keyModels.some((km) => km.id === m.id))
    .map((m) => ({ label: m.name, value: m.id }));
}

async function loadKeyAllowedModels(keyId: string) {
  if (!group.value) return;
  try {
    const models = await groupsStore.fetchKeyAllowedModels(group.value.id, keyId);
    keyAllowedModelsMap.value[keyId] = models;
  } catch {
    // silently fail
  }
}

async function onAddKeyModel(keyId: string, modelId: string) {
  if (!group.value || !modelId) return;
  keyModelsLoading.value[keyId] = true;
  try {
    await groupsStore.addKeyAllowedModel(group.value.id, keyId, modelId);
    await loadKeyAllowedModels(keyId);
    const modelName = allowedModels.value.find((m) => m.id === modelId)?.name || modelId;
    $q.notify({ type: 'positive', message: `"${modelName}" added to key` });
  } catch (e: unknown) {
    const msg = (e as { response?: { data?: { error?: string } } })?.response?.data?.error || 'Failed to add model to key';
    $q.notify({ type: 'negative', message: msg });
  } finally {
    keyModelsLoading.value[keyId] = false;
  }
}

async function onRemoveKeyModel(keyId: string, modelId: string) {
  if (!group.value) return;
  keyModelsLoading.value[keyId] = true;
  try {
    const modelName = getKeyAllowedModels(keyId).find((m) => m.id === modelId)?.name || modelId;
    await groupsStore.removeKeyAllowedModel(group.value.id, keyId, modelId);
    await loadKeyAllowedModels(keyId);
    $q.notify({ type: 'positive', message: `"${modelName}" removed from key` });
  } catch (e: unknown) {
    const msg = (e as { response?: { data?: { error?: string } } })?.response?.data?.error || 'Failed to remove model from key';
    $q.notify({ type: 'negative', message: msg });
  } finally {
    keyModelsLoading.value[keyId] = false;
  }
}

// Key server assignment helpers
function getKeyServers(keyId: string): GroupServerDetail[] {
  return keyServersMap.value[keyId] || [];
}

function keyServerOptions(keyId: string): GroupServerDetail[] {
  const assigned = getKeyServers(keyId);
  return servers.value.filter((gs) => !assigned.some((a) => a.server_id === gs.server_id));
}

async function loadKeyServers(keyId: string) {
  if (!group.value) return;
  try {
    keyServersLoading.value[keyId] = true;
    const data = await groupsStore.fetchKeyServers(group.value.id, keyId);
    keyServersMap.value[keyId] = data;
  } catch {
    keyServersMap.value[keyId] = [];
  } finally {
    keyServersLoading.value[keyId] = false;
  }
}

async function onAddKeyServer(keyId: string, serverId: string) {
  if (!group.value) return;
  keyServersLoading.value[keyId] = true;
  try {
    await groupsStore.assignKeyServer(group.value.id, keyId, serverId);
    await loadKeyServers(keyId);
    const serverName = servers.value.find((s) => s.server_id === serverId)?.server_name || serverId;
    $q.notify({ type: 'positive', message: `"${serverName}" added to key` });
  } catch (e: unknown) {
    const msg = (e as { response?: { data?: { error?: string } } })?.response?.data?.error || 'Failed to add server to key';
    $q.notify({ type: 'negative', message: msg });
    keyServersLoading.value[keyId] = false;
  }
}

async function onRemoveKeyServer(keyId: string, serverId: string) {
  if (!group.value) return;
  keyServersLoading.value[keyId] = true;
  try {
    const serverName = getKeyServers(keyId).find((s) => s.server_id === serverId)?.server_name || serverId;
    await groupsStore.removeKeyServer(group.value.id, keyId, serverId);
    await loadKeyServers(keyId);
    $q.notify({ type: 'positive', message: `"${serverName}" removed from key` });
  } catch (e: unknown) {
    const msg = (e as { response?: { data?: { error?: string } } })?.response?.data?.error || 'Failed to remove server from key';
    $q.notify({ type: 'negative', message: msg });
    keyServersLoading.value[keyId] = false;
  }
}

// Rate badge helpers
function hasNonDefaultRate(s: GroupServerDetail): boolean {
  return [s.rate_input, s.rate_output, s.rate_cache_write, s.rate_cache_read].some(
    (r) => r !== null && r !== undefined && r !== 1.0,
  );
}

function formatTokenThreshold(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(n % 1_000_000 === 0 ? 0 : 1)}M`;
  if (n >= 1000) return `${(n / 1000).toFixed(n % 1000 === 0 ? 0 : 1)}K`;
  return String(n);
}

function displayRate(s: GroupServerDetail): string {
  const rates = [s.rate_input, s.rate_output, s.rate_cache_write, s.rate_cache_read];
  const nonDefault = rates.filter((r) => r !== null && r !== undefined && r !== 1.0);
  if (nonDefault.length === 0) return '1.0';
  const allSame = nonDefault.length === rates.length && nonDefault.every((r) => r === nonDefault[0]);
  if (allSame) return String(nonDefault[0]);
  return 'custom';
}

function openRetryDialog(s: GroupServerDetail) {
  retryEditServer.value = s;
  retryForm.value = {
    retry_status_codes_str: s.retry_status_codes != null ? s.retry_status_codes.join(',') : '',
    retry_count: s.retry_count,
    retry_delay_seconds: s.retry_delay_seconds,
    non_stream_timeout_ms: s.non_stream_timeout_ms ?? null,
  };
  showRetryDialog.value = true;
}

async function saveRetryConfig() {
  if (!group.value || !retryEditServer.value) return;

  const { retry_status_codes_str, retry_count, retry_delay_seconds, non_stream_timeout_ms } = retryForm.value;
  const hasStr = retry_status_codes_str.trim() !== '';
  const hasCount = retry_count !== null && retry_count !== undefined;
  const hasDelay = retry_delay_seconds !== null && retry_delay_seconds !== undefined;
  const filledCount = [hasStr, hasCount, hasDelay].filter(Boolean).length;

  if (non_stream_timeout_ms !== null && non_stream_timeout_ms !== undefined && non_stream_timeout_ms < 1) {
    $q.notify({ type: 'negative', message: 'non_stream_timeout_ms must be >= 1' });
    return;
  }

  // Determine payload: all null (clear) or all set. The timeout is independent of
  // the retry trio and always sent as-is.
  let payload: {
    retry_status_codes: number[] | null;
    retry_count: number | null;
    retry_delay_seconds: number | null;
    non_stream_timeout_ms: number | null;
  };

  if (filledCount === 0) {
    // Clear retry config
    payload = { retry_status_codes: null, retry_count: null, retry_delay_seconds: null, non_stream_timeout_ms };
  } else if (filledCount === 3) {
    // Parse status codes
    const codes = retry_status_codes_str
      .split(',')
      .map((s) => parseInt(s.trim(), 10))
      .filter((n) => !Number.isNaN(n));
    if (codes.length === 0) {
      $q.notify({ type: 'negative', message: 'retry_status_codes must be non-empty' });
      return;
    }
    if (codes.some((c) => c < 400 || c > 599)) {
      $q.notify({ type: 'negative', message: 'retry_status_codes values must be in range 400-599' });
      return;
    }
    if ((retry_count as number) < 1) {
      $q.notify({ type: 'negative', message: 'retry_count must be >= 1' });
      return;
    }
    if ((retry_delay_seconds as number) <= 0) {
      $q.notify({ type: 'negative', message: 'retry_delay_seconds must be > 0' });
      return;
    }
    payload = {
      retry_status_codes: codes,
      retry_count: retry_count as number,
      retry_delay_seconds: retry_delay_seconds as number,
      non_stream_timeout_ms,
    };
  } else {
    $q.notify({ type: 'negative', message: 'All three retry fields must be filled or all left empty.' });
    return;
  }

  savingRetry.value = true;
  try {
    await groupsStore.updateAssignment(group.value.id, retryEditServer.value.server_id, payload);
    showRetryDialog.value = false;
    await loadGroup();
    $q.notify({ type: 'positive', message: 'Retry config saved' });
  } catch (e: unknown) {
    const msg = (e as { response?: { data?: { error?: string } } })?.response?.data?.error || 'Failed to save retry config';
    $q.notify({ type: 'negative', message: msg });
  } finally {
    savingRetry.value = false;
  }
}

function openRateModal(s: GroupServerDetail) {  rateEditServer.value = s;
  rateForm.value = {
    rate_input: s.rate_input,
    rate_output: s.rate_output,
    rate_cache_write: s.rate_cache_write,
    rate_cache_read: s.rate_cache_read,
    normalize_cache_read: s.normalize_cache_read,
  };
  showRateModal.value = true;
}

async function onSaveRates() {
  if (!group.value || !rateEditServer.value) return;
  for (const field of ['rate_input', 'rate_output', 'rate_cache_write', 'rate_cache_read'] as const) {
    if (rateForm.value[field] !== null && rateForm.value[field] !== undefined && (rateForm.value[field] as number) < 0) {
      $q.notify({ type: 'negative', message: `${field} must be non-negative` });
      return;
    }
  }
  savingRate.value = true;
  try {
    await groupsStore.updateAssignment(group.value.id, rateEditServer.value.server_id, {
      rate_input: rateForm.value.rate_input,
      rate_output: rateForm.value.rate_output,
      rate_cache_write: rateForm.value.rate_cache_write,
      rate_cache_read: rateForm.value.rate_cache_read,
      normalize_cache_read: rateForm.value.normalize_cache_read,
    });
    showRateModal.value = false;
    await loadGroup();
    $q.notify({ type: 'positive', message: 'Rates saved' });
  } catch (e: unknown) {
    const msg = (e as { response?: { data?: { error?: string } } })?.response?.data?.error || 'Failed to save rates';
    $q.notify({ type: 'negative', message: msg });
  } finally {
    savingRate.value = false;
  }
}

function subKeyRowKey(row: KeyTokenUsage) {
  return row.group_key_id ?? '__null__';
}

function onExpandSubKeyUsageRow(props: { expand: boolean; row: KeyTokenUsage }) {
  if (props.row.group_key_id == null) return;
  props.expand = !props.expand;
  if (props.expand) {
    loadKeySubscriptions(props.row.group_key_id);
    loadActivePlans();
  }
}

async function loadTtftKeys() {
  if (!group.value) return;
  if (subKeys.value.length === 0) {
    try {
      const data = await groupsStore.fetchGroupKeys(group.value.id, { limit: 100 });
      subKeys.value = data.data;
    } catch {
      // silently fail — dropdown just shows "All keys"
    }
  }
}
</script>

<style scoped lang="scss">
.disabled-server {
  opacity: 0.5;
}

.bonus-models-cell {
  align-items: center;
  background: transparent;
  border: 0;
  color: inherit;
  cursor: pointer;
  display: inline-flex;
  font: inherit;
  padding: 0;
  text-align: left;
}

.period-toggle {
  display: inline-flex;
  border: 1px solid var(--vr-border, rgba(255, 255, 255, 0.14));
  border-radius: 8px;
  overflow: hidden;
  background: transparent;
}
.period-toggle :deep(.q-btn) {
  font-size: 12px;
  padding: 4px 12px;
  min-height: 28px;
  border-radius: 0;
  border: none;
  box-shadow: none;
  color: var(--vr-text-secondary, rgba(255, 255, 255, 0.6));
  background: transparent;
}
.period-toggle :deep(.q-btn + .q-btn) {
  border-left: 1px solid var(--vr-border, rgba(255, 255, 255, 0.14));
}
.period-toggle :deep(.q-btn--active) {
  color: var(--q-primary);
  background: rgba(25, 118, 210, 0.12);
}
</style>
