import { describe, expect, it } from "vitest";
import {
  alertTone,
  describeChange,
  describeSample,
  dismissAlert,
  pushAlert,
  retryLabel,
} from "./alertBanner";
import { sampleAlert } from "./alertFixtures";

describe("alert banner helpers", () => {
  it("describes before → attempted → after per field", () => {
    expect(describeChange(sampleAlert().groups[0].miners[0])).toBe("525 → 600 → 525 MHz");
    expect(
      describeChange({ before: { fan_pct: 60 }, attempted: { fan_pct: 80, frequency_mhz: 550 }, after: null }),
    ).toBe("? → 550 → ? MHz, 60 → 80 → ? % fan");
  });

  it("describes the failing health sample", () => {
    expect(describeSample({ reachable: true, hashrate_ghs: 0, temp_c: 95.04 })).toBe("95 °C, 0 GH/s");
    expect(describeSample({ reachable: false, hashrate_ghs: null, temp_c: null })).toBe("unreachable");
    expect(describeSample(null)).toBe("");
  });

  it("keeps one alert per job and dismisses by run id", () => {
    let list = pushAlert([], sampleAlert());
    list = pushAlert(list, sampleAlert({ headline: "updated" }));
    expect(list).toHaveLength(1);
    expect(list[0].headline).toBe("updated");
    list = pushAlert(list, sampleAlert({ run_id: "job-2" }));
    expect(list.map((a) => a.run_id)).toEqual(["job-2", "job-1"]);
    expect(dismissAlert(list, "job-2").map((a) => a.run_id)).toEqual(["job-1"]);
  });

  it("tone escalates when a rollback failed; retry is labelled as a dry run", () => {
    expect(alertTone(sampleAlert())).toBe("warn");
    expect(alertTone(sampleAlert({ rollback_failed: 1 }))).toBe("danger");
    expect(retryLabel(sampleAlert())).toBe("Retry 3 miners (dry run)");
  });
});
