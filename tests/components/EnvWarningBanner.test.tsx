import { render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { EnvWarningBanner } from "@/components/env/EnvWarningBanner";
import type { EnvConflict } from "@/types/env";

vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (key: string, options?: { count?: number }) => {
      if (key === "env.warning.title") return "检测到系统环境变量冲突";
      if (key === "env.warning.description")
        return `发现 ${options?.count ?? 0} 个环境变量可能会覆盖您的配置`;
      if (key === "env.actions.expand") return "查看详情";
      if (key === "env.actions.collapse") return "收起";
      return key;
    },
  }),
}));

describe("EnvWarningBanner", () => {
  it("renders nothing when conflicts list is empty", () => {
    const { container } = render(
      <EnvWarningBanner
        conflicts={[]}
        onDismiss={vi.fn()}
        onDeleted={vi.fn()}
      />,
    );
    expect(container.firstChild).toBeNull();
  });

  it("renders nothing when all conflicts have empty or whitespace values", () => {
    const emptyConflicts: EnvConflict[] = [
      {
        varName: "ANTHROPIC_API_KEY",
        varValue: "",
        sourceType: "system",
        sourcePath: "Process Environment",
      },
      {
        varName: "OPENAI_API_KEY",
        varValue: "   ",
        sourceType: "system",
        sourcePath: "Process Environment",
      },
      {
        varName: "GEMINI_API_KEY",
        varValue: "",
        sourceType: "system",
        sourcePath: "Process Environment",
      },
    ];

    const { container } = render(
      <EnvWarningBanner
        conflicts={emptyConflicts}
        onDismiss={vi.fn()}
        onDeleted={vi.fn()}
      />,
    );
    expect(container.firstChild).toBeNull();
  });

  it("renders warning banner and counts only conflicts with actual values", () => {
    const mixedConflicts: EnvConflict[] = [
      {
        varName: "ANTHROPIC_API_KEY",
        varValue: "",
        sourceType: "system",
        sourcePath: "Process Environment",
      },
      {
        varName: "OPENAI_API_KEY",
        varValue: "sk-proj-actual-key",
        sourceType: "system",
        sourcePath: "Process Environment",
      },
    ];

    render(
      <EnvWarningBanner
        conflicts={mixedConflicts}
        onDismiss={vi.fn()}
        onDeleted={vi.fn()}
      />,
    );

    expect(screen.getByText("检测到系统环境变量冲突")).toBeInTheDocument();
    expect(
      screen.getByText("发现 1 个环境变量可能会覆盖您的配置"),
    ).toBeInTheDocument();
  });
});
