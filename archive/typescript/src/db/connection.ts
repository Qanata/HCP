import pg from "pg";
import { config } from "../config.js";

let pool: pg.Pool | null = null;

export function getPool(): pg.Pool {
  if (pool) return pool;
  pool = new pg.Pool({ connectionString: config.databaseUrl, ssl: { rejectUnauthorized: false } });
  return pool;
}

/** Override the pool (for testing). */
export function setPool(instance: pg.Pool): void {
  pool = instance;
}

export async function closePool(): Promise<void> {
  if (pool) {
    await pool.end();
    pool = null;
  }
}

/** Convenience wrapper — runs a query on the pool. */
export async function query<T extends pg.QueryResultRow = pg.QueryResultRow>(
  sql: string,
  params?: unknown[]
): Promise<pg.QueryResult<T>> {
  return getPool().query<T>(sql, params);
}
