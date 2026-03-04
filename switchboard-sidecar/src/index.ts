/**
 * Switchboard Managed Update Sidecar for MrOracle
 *
 * Polls fetchManagedUpdateIxs and writes serialized instructions as JSON lines
 * to stdout. MrOracle (Rust) spawns this as a child process and reads the output.
 *
 * All logging goes to stderr so it doesn't interfere with the JSON protocol.
 *
 * Follows the exact pattern from sb-on-demand-examples/solana/feeds/basic/scripts/managedUpdate.ts
 */

import * as sb from "@switchboard-xyz/on-demand";
import { OracleQuote } from "@switchboard-xyz/on-demand";
import { CrossbarClient } from "@switchboard-xyz/common";
import {
  PublicKey,
  TransactionInstruction,
} from "@solana/web3.js";

// ---------------------------------------------------------------------------
// Config — all from environment variables set by the Rust parent process
// ---------------------------------------------------------------------------

const RPC_URL = process.env.SWITCHBOARD_RPC_URL;
const PAYER_KEYPAIR_PATH = process.env.SWITCHBOARD_PAYER_KEYPAIR_PATH;
const CROSSBAR_URL =
  process.env.SWITCHBOARD_CROSSBAR_URL || "https://crossbar.switchboard.xyz";
const QUEUE_PUBKEY_STR = process.env.SWITCHBOARD_QUEUE_PUBKEY;
const FEED_HASHES_RAW = process.env.SWITCHBOARD_FEED_HASHES || "";
const POLL_MS = parseInt(process.env.SWITCHBOARD_POLL_MS || "2000", 10);
const INSTRUCTION_IDX = parseInt(
  process.env.SWITCHBOARD_INSTRUCTION_IDX || "2",
  10
);

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function log(msg: string): void {
  process.stderr.write(`[switchboard-sidecar] ${msg}\n`);
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

interface SerializedAccountMeta {
  pubkey: string;
  is_signer: boolean;
  is_writable: boolean;
}

interface SerializedInstruction {
  program_id: string;
  accounts: SerializedAccountMeta[];
  data: string; // base64
}

interface SidecarOutput {
  ed25519_ix: SerializedInstruction;
  quote_store_ix: SerializedInstruction;
  quote_account: string; // base58
}

function serializeInstruction(ix: TransactionInstruction): SerializedInstruction {
  return {
    program_id: ix.programId.toBase58(),
    accounts: ix.keys.map((k) => ({
      pubkey: k.pubkey.toBase58(),
      is_signer: k.isSigner,
      is_writable: k.isWritable,
    })),
    data: Buffer.from(ix.data).toString("base64"),
  };
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

async function main(): Promise<void> {
  // Validate required env vars
  if (!RPC_URL) throw new Error("SWITCHBOARD_RPC_URL is required");
  if (!PAYER_KEYPAIR_PATH)
    throw new Error("SWITCHBOARD_PAYER_KEYPAIR_PATH is required");
  if (!QUEUE_PUBKEY_STR)
    throw new Error("SWITCHBOARD_QUEUE_PUBKEY is required");

  const feedHashes = FEED_HASHES_RAW.split(",").filter(Boolean);
  if (feedHashes.length === 0)
    throw new Error("SWITCHBOARD_FEED_HASHES is required (comma-separated hex hashes)");

  // Use AnchorUtils.loadEnv() — the canonical way from the sb-on-demand examples.
  // It reads ANCHOR_WALLET and ANCHOR_PROVIDER_URL to build the Anchor provider & program.
  process.env.ANCHOR_WALLET = PAYER_KEYPAIR_PATH;
  process.env.ANCHOR_PROVIDER_URL = RPC_URL;

  const env = await sb.AnchorUtils.loadEnv();

  if (!env.program) {
    throw new Error(
      "sb.AnchorUtils.loadEnv() returned null program — is @switchboard-xyz/on-demand installed correctly?"
    );
  }

  // Build Queue and CrossbarClient — override auto-detected values with our config
  const queuePubkey = new PublicKey(QUEUE_PUBKEY_STR);
  const queue = new sb.Queue(env.program, queuePubkey);
  const crossbar = new CrossbarClient(CROSSBAR_URL);

  // Derive the canonical SwitchboardQuote PDA from queue + feed hashes
  const [quoteAccount] = OracleQuote.getCanonicalPubkey(
    queuePubkey,
    feedHashes
  );

  log(`Started: feeds=${feedHashes.length}, poll=${POLL_MS}ms, instructionIdx=${INSTRUCTION_IDX}`);
  log(`Queue: ${queuePubkey.toBase58()}`);
  log(`Quote account: ${quoteAccount.toBase58()}`);
  log(`Payer: ${env.keypair.publicKey.toBase58()}`);
  log(`Feed hashes: ${feedHashes.join(", ")}`);

  // Main polling loop — fetch managed update instructions and emit JSON lines
  while (true) {
    try {
      const instructions = await queue.fetchManagedUpdateIxs(
        crossbar,
        feedHashes,
        {
          instructionIdx: INSTRUCTION_IDX,
          payer: env.keypair.publicKey,
        }
      );

      if (instructions.length < 2) {
        log(
          `fetchManagedUpdateIxs returned ${instructions.length} instructions, expected >= 2`
        );
      } else {
        const output: SidecarOutput = {
          ed25519_ix: serializeInstruction(instructions[0]),
          quote_store_ix: serializeInstruction(instructions[1]),
          quote_account: quoteAccount.toBase58(),
        };

        // Write a single JSON line to stdout — the Rust parent reads this
        process.stdout.write(JSON.stringify(output) + "\n");
      }
    } catch (err: any) {
      log(`fetchManagedUpdateIxs error: ${err?.message || err}`);
    }

    await sleep(POLL_MS);
  }
}

main().catch((err) => {
  log(`Fatal: ${err}`);
  process.exit(1);
});
