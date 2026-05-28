<script lang="ts">
  import type { IconName } from "./iconNames";
  import LucideBrain from "~icons/lucide/brain";
  import LucideMap from "~icons/lucide/map";
  import LucideGitBranch from "~icons/lucide/git-branch";
  import LucideSun from "~icons/lucide/sun";
  import LucideMoon from "~icons/lucide/moon";
  import LucideRefreshCw from "~icons/lucide/refresh-cw";
  import LucideX from "~icons/lucide/x";
  import LucideSettings from "~icons/lucide/settings";
  import LucidePlus from "~icons/lucide/plus";
  import LucideCheck from "~icons/lucide/check";
  import LucidePin from "~icons/lucide/pin";
  import LucideUndo2 from "~icons/lucide/undo-2";

  interface Props {
    name: IconName;
    size?: number;
    /** Optional aria-label for screen readers. */
    label?: string;
  }
  let { name, size = 16, label }: Props = $props();

  const ICONS: Record<IconName, any> = {
    memory: LucideBrain,
    map: LucideMap,
    branches: LucideGitBranch,
    sun: LucideSun,
    moon: LucideMoon,
    refresh: LucideRefreshCw,
    close: LucideX,
    settings: LucideSettings,
    plus: LucidePlus,
    check: LucideCheck,
    x: LucideX,
    pin: LucidePin,
    undo: LucideUndo2,
  };

  let Comp = $derived(ICONS[name]);
</script>

<Comp
  width={size}
  height={size}
  aria-label={label ?? name}
  style="display: inline-block; vertical-align: -2px;"
/>
