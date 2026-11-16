function envOr(key: string, fallback: string): string {
  return process.env[key] ?? fallback;
}

export const config = {
  port: parseInt(envOr("HCP_PORT", "3100"), 10),
  databaseUrl: envOr("DATABASE_URL", "postgresql://localhost:5432/hcp"),
  baseUrl: envOr("HCP_BASE_URL", "http://localhost:3100"),
  slackBotToken: process.env["SLACK_BOT_TOKEN"] ?? "",
  timeoutPollIntervalMs: 10_000,
} as const;
