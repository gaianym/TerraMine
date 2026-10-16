<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { onMount } from "svelte";
  import type { CredentialSummary, FirmwareKind } from "./types";

  const LABEL: Record<FirmwareKind, string> = {
    "antminer-stock": "Antminer stock (digest)",
    braiins: "Braiins OS",
    luxos: "LuxOS",
    vnish: "VNish",
    whatsminer: "Whatsminer",
  };

  let rows = $state<CredentialSummary[]>([]);
  let editing = $state<FirmwareKind | null>(null);
  let username = $state("");
  let password = $state("");
  let msg = $state<string | null>(null);

  async function load() {
    try {
      rows = await invoke<CredentialSummary[]>("list_miner_credentials");
    } catch (e) {
      msg = `Keychain unavailable: ${e}`;
    }
  }
  onMount(load);

  function edit(r: CredentialSummary) {
    editing = r.firmware;
    username = r.username ?? "";
    password = "";
    msg = null;
  }

  async function save() {
    if (!editing) return;
    try {
      await invoke("set_miner_credentials", { firmware: editing, username, password });
      password = "";
      editing = null;
      msg = "Saved to OS keychain";
      await load();
    } catch (e) {
      msg = String(e);
    }
  }

  async function remove(f: FirmwareKind) {
    try {
      await invoke("delete_miner_credentials", { firmware: f });
      await load();
    } catch (e) {
      msg = String(e);
    }
  }
</script>

<section class="rounded-xl border border-zinc-800 bg-zinc-900/40 p-4 text-xs">
  <h2 class="mb-1 text-sm font-medium text-zinc-300">Miner credentials</h2>
  <p class="mb-3 text-zinc-500">
    Stored in the OS keychain, never in app settings. Without a saved login, stock Antminers use the
    factory default.
  </p>
  <table class="w-full">
    <tbody>
      {#each rows as r (r.firmware)}
        <tr class="border-t border-zinc-800">
          <td class="py-1.5">{LABEL[r.firmware]}</td>
          <td class="py-1.5 text-zinc-400">{r.username ? `${r.username} · ${r.has_password ? "password set" : "no password"}` : "not set"}</td>
          <td class="py-1.5 text-right">
            <button type="button" class="rounded border border-zinc-700 px-2 py-0.5 hover:bg-zinc-800" onclick={() => edit(r)}>Edit</button>
            {#if r.username}
              <button type="button" class="ml-1 rounded border border-zinc-700 px-2 py-0.5 hover:bg-zinc-800" onclick={() => remove(r.firmware)}>Remove</button>
            {/if}
          </td>
        </tr>
      {/each}
    </tbody>
  </table>
  {#if editing}
    <div class="mt-3 grid grid-cols-2 gap-2">
      <label class="flex flex-col gap-1">Username
        <input class="rounded border border-zinc-700 bg-zinc-950 px-2 py-1" autocomplete="off" bind:value={username} />
      </label>
      <label class="flex flex-col gap-1">Password
        <input class="rounded border border-zinc-700 bg-zinc-950 px-2 py-1" type="password" autocomplete="new-password" bind:value={password} />
      </label>
      <div class="col-span-2 flex justify-end gap-2">
        <button type="button" class="rounded border border-zinc-700 px-2 py-1" onclick={() => (editing = null)}>Cancel</button>
        <button type="button" class="rounded border border-zinc-600 bg-zinc-100 px-2 py-1 text-zinc-900" onclick={save}>Save</button>
      </div>
    </div>
  {/if}
  {#if msg}<p class="mt-2 text-zinc-400">{msg}</p>{/if}
</section>
