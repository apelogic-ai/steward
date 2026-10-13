import { describe, expect, test } from "bun:test";
import { renderToStaticMarkup } from "react-dom/server";

import { DurationInput, MoneyInput, formatCount, formatDuration, formatMoney } from "./presentation-values";

describe("shared presentation values", () => {
  test("formats money with two decimals and its supplied currency", () => {
    expect(formatMoney("2", "USD")).toBe("2.00 USD");
    expect(formatMoney("0.1", "EUR")).toBe("0.10 EUR");
    expect(formatMoney("0", "USD")).toBe("0.00 USD");
    expect(formatMoney("0.0042", "USD")).toBe("< 0.01 USD");
    expect(formatMoney("10.004", "USD")).toBe("10.00 USD");
    expect(formatMoney("10.005", "USD")).toBe("10.01 USD");
    expect(formatMoney("999999999999999999.995", "ABC")).toBe("1000000000000000000.00 ABC");
    expect(formatMoney(".5", "USD")).toBe("0.50 USD");
    expect(formatMoney("", "USD")).toBe("Not set");
    expect(formatMoney("not-money", "USD")).toBe("—");
  });

  test("formats wire durations and minute limits consistently", () => {
    expect(formatDuration("15m")).toBe("15 min");
    expect(formatDuration("90m")).toBe("1 h 30 min");
    expect(formatDuration("2h")).toBe("2 h");
    expect(formatDuration("60", "minutes")).toBe("1 h");
    expect(formatDuration("60", "minutes", "m")).toBe("60 min");
    expect(formatDuration("90m", "wire", "m")).toBe("90 min");
    expect(formatDuration("24h", "wire", "h")).toBe("24 h");
  });

  test("formats counts with their noun", () => {
    expect(formatCount(1, "model")).toBe("1 model");
    expect(formatCount(3, "tool")).toBe("3 tools");
  });

  test("renders numeric inputs with explicit units", () => {
    const money = renderToStaticMarkup(<MoneyInput currency="USD" onChange={() => undefined} value="2" />);
    const duration = renderToStaticMarkup(<DurationInput onChange={() => undefined} value="15m" />);
    expect(money).toContain('type="number"');
    expect(money).toContain("USD");
    expect(money).toContain("aria-describedby");
    expect(money).toContain("focus-within:");
    expect(money).not.toContain('aria-hidden="true"');
    expect(duration).toContain('type="number"');
    expect(duration).toContain('step="1"');
    expect(duration).toContain("min");
    expect(duration).toContain("aria-describedby");
    expect(duration).toContain("focus-within:");
    expect(duration).not.toContain('value="15m"');
  });
});
