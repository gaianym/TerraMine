<script lang="ts">
  const INT_MAX = Number.MAX_SAFE_INTEGER;

  let {
    label,
    class: className = "",
    value = $bindable(),
    min,
    max = INT_MAX,
  }: {
    label: string;
    class?: string;
    value: number;
    min: number;
    max?: number;
  } = $props();

  let inputEl = $state<HTMLInputElement | null>(null);
  let local = $state(String(value));

  const hi = $derived(max ?? INT_MAX);

  function clampFull(n: number): number {
    let x = Number.isFinite(n) ? Math.trunc(n) : min;
    x = Math.min(hi, x);
    return Math.max(min, x);
  }

  /** While typing: allow below min until blur; enforce max only for absurd input. */
  function clampUpperOnly(n: number): number {
    if (!Number.isFinite(n)) return value;
    return Math.min(hi, Math.trunc(Math.max(n, 0)));
  }

  $effect(() => {
    void value;
    if (typeof document !== "undefined" && inputEl && document.activeElement === inputEl)
      return;
    local = String(value);
  });

  function handleInput(ev: Event) {
    const digits = (ev.currentTarget as HTMLInputElement).value.replace(/\D/g, "");
    local = digits;
    if (digits === "") return;
    const n = parseInt(digits, 10);
    if (!Number.isFinite(n)) return;
    value = clampUpperOnly(n);
    if (String(value) !== digits) local = String(value);
  }

  function handleBlur() {
    const digits = local.replace(/\D/g, "");
    if (digits === "") {
      local = String(value);
      return;
    }
    value = clampFull(parseInt(digits, 10));
    local = String(value);
  }
</script>

<label class="flex flex-col gap-1 {className}">
  <span class="text-zinc-500">{label}</span>
  <input
    bind:this={inputEl}
    class="rounded bg-zinc-950 px-2 py-1 font-mono tabular-nums"
    type="text"
    inputmode="numeric"
    autocomplete="off"
    spellcheck="false"
    bind:value={local}
    oninput={handleInput}
    onblur={handleBlur}
  />
</label>
