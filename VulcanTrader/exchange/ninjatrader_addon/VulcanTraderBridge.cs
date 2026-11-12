// VulcanTraderBridge.cs - NinjaScript AddOn: TCP bridge between NinjaTrader 8
// and VulcanTrader's "ninjatrader" exchange (VulcanTrader/exchange/ninjatrader.py).
//
// =====================================================================
// INSTALL (one-time, inside NinjaTrader 8)
// =====================================================================
//   1. Open NT8 -> Tools -> Edit NinjaScript -> AddOn...  (NinjaScript Editor)
//   2. File -> New AddOn... name it "VulcanTraderBridge" (must match the
//      class name below).
//   3. Replace the generated file's contents with this entire file.
//   4. Press F5 (Compile). Fix any compiler errors the Editor reports -
//      see the NOTE comments below for the exact spots most likely to need
//      a small signature fix (member names drift slightly between NT8
//      versions and this was written from documentation, not compiled
//      against a live install).
//   5. Restart NT8 (AddOns are loaded at startup) with whichever
//      connection(s) you want reachable already configured: Sim101 is
//      always available; for "live" connect your real brokerage connection;
//      for "replay" start a Market Replay connection. The account name
//      VulcanTrader is configured with (`ninjatrader_account` in its
//      config, e.g. "Sim101") must exist in NT8's Account list - `Account.All`
//      below - at the time a bot connects, i.e. its connection must already
//      be started.
//   6. Point VulcanTrader's "ninjatrader" exchange config at this machine's
//      IP (or 127.0.0.1 if the bot runs on the same machine as NT8) and
//      ListenPort below (default 8755).
//
// =====================================================================
// PROTOCOL
// =====================================================================
// One TCP connection per VulcanTrader exchange instance (bots sharing one
// (host, port, account) in a single_process_mode fleet share one connection
// - see ninjatrader.py's registry). Newline-delimited JSON, UTF-8, one
// message object per line, both directions.
//
// Python -> NT8:
//   {"type":"hello","account":"Sim101","client":"VulcanTrader"}
//   {"type":"subscribe_bars","instrument":"ES 12-25","timeframe":"1m"}
//   {"type":"subscribe_ticks","instrument":"ES 12-25"}
//   {"type":"backfill_bars","req_id":"...","instrument":"ES 12-25","timeframe":"1m","since_ms":...,"until_ms":...}
//   {"type":"place_order","order_id":"...","instrument":"ES 12-25","action":"BUY"|"SELL",
//    "order_type":"MARKET"|"LIMIT","quantity":1,"limit_price":...|null,"stop_price":...|null,
//    "tif":"GTC"|"DAY"|"IOC"|"FOK","reduce_only":false,"account":"Sim101"}
//   {"type":"cancel_order","order_id":"..."}
//
// NT8 -> Python:
//   {"type":"hello_ack","account_exists":true}
//   {"type":"bar","instrument":"ES 12-25","timeframe":"1m","ts_ms":...,"open":...,"high":...,"low":...,"close":...,"volume":...}
//   {"type":"tick","instrument":"ES 12-25","ts_ms":...,"price":...}
//   {"type":"backfill_result","req_id":"...","bars":[[ts_ms,o,h,l,c,v], ...]}
//   {"type":"order_update","order_id":"...","nt_order_id":"...","status":"Submitted"|"Accepted"|"Working"|
//    "PartFilled"|"Filled"|"Cancelled"|"Rejected","instrument":"...","filled_qty":...,"avg_fill_price":...,
//    "remaining_qty":...,"reject_reason":"..."|null}
//   {"type":"account_state","cash_value":...,"net_liquidation":...,"buying_power":...,
//    "positions":[{"instrument":"...","quantity":...,"avg_price":...,"market_position":"Long"|"Short"|"Flat"}]}
//   {"type":"error","message":"..."}
//
// =====================================================================
// KNOWN RISK / NOT YET COMPILED
// =====================================================================
// Written from NinjaTrader 8's documented NinjaScript API (Cbi.Account,
// Cbi.Order, Data.BarsRequest) but never compiled against a live NT8
// install - the NinjaScript Editor's own compiler (step 4 above) is the
// first real check either side of this has had. Spots flagged "VERIFY"
// below are the ones most likely to need a one-line fix if a member name
// has drifted between NT8 versions. Tick-level data is intentionally NOT
// implemented via raw MarketData subscription (that API differs more
// between an AddOn context and a chart/indicator context than BarsRequest
// does) - "tick" messages are instead derived from whichever bar timeframe
// is subscribed finest, pushed on every live bar update using that bar's
// close. This is documented as a deliberate simplification, not an
// oversight - see ninjatrader.py's module docstring for the Python-side
// equivalent (its get_last_price() already falls back to "latest bar
// close" when no tick has arrived, so this costs nothing there).
//
// Front-month rollover, on-exchange stoploss orders, and OCO bracket
// management are deliberately NOT implemented here - see ninjatrader.py's
// module docstring for the same list of documented gaps on the Python side.

#region Using declarations
using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Linq;
using System.Net;
using System.Net.Sockets;
using System.Text;
using System.Threading;
using NinjaTrader.Cbi;
using NinjaTrader.Data;
using NinjaTrader.NinjaScript;
#endregion

namespace NinjaTrader.NinjaScript.AddOns
{
	public class VulcanTraderBridge : NinjaTrader.NinjaScript.AddOnBase
	{
		// Change this (and recompile) if 8755 is already in use on your
		// machine. Must match "ninjatrader_port" in the bot's config.
		private const int ListenPort = 8755;

		private TcpListener listener;
		private Thread listenerThread;
		private Timer accountStateTimer;
		private volatile bool stopping;

		private readonly List<ClientSession> clients = new List<ClientSession>();
		private readonly object clientsLock = new object();

		// instrument|timeframe -> shared subscription (dedupes NT8-side
		// BarsRequest objects when multiple bots want the same data).
		private readonly ConcurrentDictionary<string, BarsSubscription> barSubs =
			new ConcurrentDictionary<string, BarsSubscription>();

		// order_id (our GUID, carried as the NT Order's Name) -> owning session.
		private readonly ConcurrentDictionary<string, ClientSession> orderOwners =
			new ConcurrentDictionary<string, ClientSession>();
		// order_id -> live NT Order object, for Cancel().
		private readonly ConcurrentDictionary<string, Order> ordersById =
			new ConcurrentDictionary<string, Order>();
		// order_id -> the exact instrument string the bot originally sent in
		// place_order. Confirmed against a real NT8 compile+run: NT8
		// canonicalizes instrument names internally (a request for "MNQ 12-26"
		// comes back as order.Instrument.FullName == "MNQ DEC26") - echoing
		// THAT back instead of what the bot asked for would silently break the
		// bot's own pair-string bookkeeping (it correlates orders/trades by the
		// exact string it submitted), so every order_update must report the
		// originally-requested string, not NT8's own canonical name.
		private readonly ConcurrentDictionary<string, string> orderRequestedInstrument =
			new ConcurrentDictionary<string, string>();

		// Same problem as orderRequestedInstrument, for Account.Positions
		// instead of orders: a position has no order_id to carry a
		// requested-string tag, so Positions always reports NT8's own
		// canonical Instrument.FullName with no memory of what string the bot
		// used to get there. Populated every time this process resolves an
		// instrument (HandlePlaceOrder, BarsSubscription's constructor) -
		// canonical name -> the string the bot originally asked for - and
		// consulted in BuildAccountStatePayload so a position shows up under
		// the SAME string the bot would recognize, not NT8's display name.
		// Best-effort: a position that existed before this process resolved
		// that instrument at least once (prior session, opened by a human,
		// opened by a different bot) falls back to the canonical name - a
		// documented gap, not silently wrong.
		private readonly ConcurrentDictionary<string, string> instrumentCanonicalToRequested =
			new ConcurrentDictionary<string, string>();

		// Accounts we've subscribed OrderUpdate/ExecutionUpdate on, so we
		// don't double-subscribe if multiple clients use the same account.
		private readonly ConcurrentDictionary<string, Account> wiredAccounts =
			new ConcurrentDictionary<string, Account>();

		protected override void OnStateChange()
		{
			if (State == State.SetDefaults)
			{
				Name = "VulcanTraderBridge";
				Description = "TCP bridge for the VulcanTrader bot (order routing + market data).";
			}
			else if (State == State.Active)
			{
				stopping = false;
				listener = new TcpListener(IPAddress.Any, ListenPort);
				listener.Start();
				listenerThread = new Thread(AcceptLoop) { IsBackground = true, Name = "VulcanTraderBridgeListener" };
				listenerThread.Start();

				// Pushes account_state to every connected client every 2s.
				// Simpler and less API-surface-risk than wiring
				// Account.AccountItemUpdate for push-on-change.
				accountStateTimer = new Timer(PushAccountStates, null, 2000, 2000);

				Log("VulcanTraderBridge listening on port " + ListenPort, LogLevel.Information);
			}
			else if (State == State.Terminated)
			{
				stopping = true;
				try { accountStateTimer?.Dispose(); } catch { }
				try { listener?.Stop(); } catch { }

				List<ClientSession> toClose;
				lock (clientsLock) { toClose = new List<ClientSession>(clients); clients.Clear(); }
				foreach (var c in toClose)
				{
					try { c.Close(); } catch { }
				}
				foreach (var sub in barSubs.Values)
				{
					try { sub.Dispose(); } catch { }
				}
				barSubs.Clear();
			}
		}

		private void AcceptLoop()
		{
			while (!stopping)
			{
				try
				{
					TcpClient tcpClient = listener.AcceptTcpClient();
					var session = new ClientSession(this, tcpClient);
					lock (clientsLock) { clients.Add(session); }
					var t = new Thread(session.ReadLoop) { IsBackground = true, Name = "VulcanTraderBridgeClient" };
					t.Start();
				}
				catch (SocketException)
				{
					// listener.Stop() during Terminated throws this - expected, just exit.
					break;
				}
				catch (Exception ex)
				{
					if (!stopping)
						Log("VulcanTraderBridge accept loop error: " + ex, LogLevel.Error);
				}
			}
		}

		internal void RemoveClient(ClientSession session)
		{
			lock (clientsLock) { clients.Remove(session); }
			foreach (var kvp in orderOwners.Where(k => k.Value == session).ToList())
				orderOwners.TryRemove(kvp.Key, out _);
			// Dispose and drop any subscription this was the last subscriber
			// of - see RemoveSubscriber's docstring for why leaving these
			// running forever is a real, confirmed resource leak, not just
			// theoretical cleanliness.
			foreach (var kvp in barSubs.ToList())
			{
				if (kvp.Value.RemoveSubscriber(session))
				{
					if (barSubs.TryRemove(kvp.Key, out var removed))
					{
						try { removed.Dispose(); } catch { }
					}
				}
			}
		}

		// -- account resolution / order routing ---------------------------------

		internal Account ResolveAccount(string name)
		{
			// VERIFY: Account.All is the documented static collection of every
			// account NT8 currently knows about (Sim101 always present; a live
			// or Market Replay account appears once that connection is started).
			return Account.All.FirstOrDefault(a => a.Name == name);
		}

		private void EnsureAccountWired(Account account)
		{
			if (account == null) return;
			// TryAdd only succeeds the first time any client uses this account -
			// that's the "subscribe exactly once" guard, regardless of how many
			// bots/clients share it.
			if (wiredAccounts.TryAdd(account.Name, account))
			{
				account.OrderUpdate += OnOrderUpdate;
				account.ExecutionUpdate += OnExecutionUpdate;
			}
		}

		internal void HandlePlaceOrder(ClientSession session, Dictionary<string, object> msg)
		{
			string orderId = Str(msg, "order_id");
			string instrumentName = Str(msg, "instrument");
			string accountName = Str(msg, "account") ?? session.Account;
			try
			{
				Account account = ResolveAccount(accountName);
				if (account == null)
				{
					SendOrderUpdate(session, orderId, "Rejected", instrumentName, 0, null, 0,
						"Account '" + accountName + "' not found/connected in NT8.", null);
					return;
				}
				EnsureAccountWired(account);

				Instrument instrument = Instrument.GetInstrument(instrumentName);
				if (instrument == null)
				{
					SendOrderUpdate(session, orderId, "Rejected", instrumentName, 0, null, 0,
						"Instrument '" + instrumentName + "' could not be resolved by NT8 - check spelling/contract month.", null);
					return;
				}
				if (instrument.FullName != null)
					instrumentCanonicalToRequested[instrument.FullName] = instrumentName;

				string action = Str(msg, "action");
				OrderAction orderAction = action == "SELL" ? OrderAction.Sell : OrderAction.Buy;
				string orderTypeStr = Str(msg, "order_type");
				OrderType orderType = orderTypeStr == "LIMIT" ? OrderType.Limit : OrderType.Market;
				double quantity = Dbl(msg, "quantity", 1);
				double? limitPrice = DblOrNull(msg, "limit_price");
				double? stopPrice = DblOrNull(msg, "stop_price");
				TimeInForce tif = MapTif(Str(msg, "tif"));

				// Account.CreateOrder's parameter order is (instrument, action,
				// orderType, orderEntry, timeInForce, quantity, limitPrice,
				// stopPrice, ociId, name, gtdDate, customOrder) - confirmed
				// against a real NT8 compile (no error here). `name` (orderId
				// here) is what OnOrderUpdate below matches back against - it
				// must survive as Order.Name on the returned object.
				Order order = account.CreateOrder(
					instrument,
					orderAction,
					orderType,
					OrderEntry.Manual,
					tif,
					(int)Math.Round(quantity),
					limitPrice ?? 0.0,
					stopPrice ?? 0.0,
					string.Empty,
					orderId,
					DateTime.MinValue,
					null);

				ordersById[orderId] = order;
				orderOwners[orderId] = session;
				orderRequestedInstrument[orderId] = instrumentName;

				account.Submit(new[] { order });
				SendOrderUpdate(session, orderId, "Submitted", instrumentName, 0, null, quantity, null, order.OrderId);
			}
			catch (Exception ex)
			{
				SendOrderUpdate(session, orderId, "Rejected", instrumentName, 0, null, 0, ex.Message, null);
			}
		}

		internal void HandleCancelOrder(ClientSession session, Dictionary<string, object> msg)
		{
			string orderId = Str(msg, "order_id");
			if (orderId != null && ordersById.TryGetValue(orderId, out Order order))
			{
				try
				{
					Account account = order.Account;
					account.Cancel(new[] { order });
				}
				catch (Exception ex)
				{
					SendError(session, "cancel_order failed for " + orderId + ": " + ex.Message);
				}
			}
		}

		// OrderEventArgs.Order's member names (OrderState, Filled,
		// AverageFillPrice, Name) confirmed against a real NT8 compile+run.
		private void OnOrderUpdate(object sender, OrderEventArgs e)
		{
			Order order = e.Order;
			string orderId = order?.Name;
			if (string.IsNullOrEmpty(orderId) || !orderOwners.TryGetValue(orderId, out ClientSession session))
				return;

			string status = order.OrderState.ToString();
			double filled = order.Filled;
			double? avgPrice = order.Filled > 0 ? (double?)order.AverageFillPrice : null;
			double remaining = Math.Max(order.Quantity - order.Filled, 0);
			string reject = order.OrderState == OrderState.Rejected ? "Rejected by NT8/broker" : null;

			// Report the instrument string the bot originally requested, not
			// order.Instrument.FullName - confirmed live that NT8 canonicalizes
			// names internally ("MNQ 12-26" in, "MNQ DEC26" back out), which
			// would otherwise silently break the bot's own pair-string
			// bookkeeping. Falls back to NT8's name only if we somehow don't
			// have the original on file (should not happen in practice).
			string instrumentForBot = orderRequestedInstrument.TryGetValue(orderId, out var requested)
				? requested
				: order.Instrument?.FullName;

			SendOrderUpdate(session, orderId, status, instrumentForBot, filled, avgPrice, remaining, reject, order.OrderId);

			// Nothing will ever Cancel() or re-report on an order once it's
			// terminal - evict so a long-running AddOn doesn't accumulate every
			// order ever placed for the life of the NT8 process.
			bool isTerminal = order.OrderState == OrderState.Filled
				|| order.OrderState == OrderState.Cancelled
				|| order.OrderState == OrderState.Rejected;
			if (isTerminal)
			{
				ordersById.TryRemove(orderId, out _);
				orderRequestedInstrument.TryRemove(orderId, out _);
			}
		}

		private void OnExecutionUpdate(object sender, ExecutionEventArgs e)
		{
			// Execution-level detail isn't separately pushed - OnOrderUpdate
			// already fires with updated Filled/AverageFillPrice on every fill
			// (NT8 raises OrderUpdate alongside ExecutionUpdate for the same
			// event), so this handler intentionally does nothing extra. Kept
			// as an explicit no-op (rather than not subscribing at all) so a
			// future addition (e.g. per-fill logging) has an obvious anchor.
		}

		private void SendOrderUpdate(ClientSession session, string orderId, string status, string instrument,
			double filledQty, double? avgFillPrice, double remainingQty, string rejectReason, string ntOrderId)
		{
			var sb = new StringBuilder();
			sb.Append("{\"type\":\"order_update\"");
			AppendStr(sb, "order_id", orderId);
			AppendStr(sb, "nt_order_id", ntOrderId);
			AppendStr(sb, "status", status);
			AppendStr(sb, "instrument", instrument);
			AppendNum(sb, "filled_qty", filledQty);
			AppendNumOrNull(sb, "avg_fill_price", avgFillPrice);
			AppendNum(sb, "remaining_qty", remainingQty);
			AppendStrOrNull(sb, "reject_reason", rejectReason);
			sb.Append("}");
			session.Send(sb.ToString());
		}

		private void SendError(ClientSession session, string message)
		{
			var sb = new StringBuilder();
			sb.Append("{\"type\":\"error\"");
			AppendStr(sb, "message", message);
			sb.Append("}");
			session.Send(sb.ToString());
		}

		// -- bars / ticks ---------------------------------------------------------

		// Both subscribe handlers catch and report: BarsSubscription's constructor
		// throws for an instrument NT8 can't resolve or an unmapped timeframe, and
		// an exception escaping Dispatch() ends ClientSession.ReadLoop - i.e. one
		// bad symbol from one bot dropped the whole shared connection for every
		// bot in that Python process (seen live 2026-11-11 with bare roots and
		// CFD symbols in pair_whitelist: constant connect/drop churn).
		internal void HandleSubscribeBars(ClientSession session, Dictionary<string, object> msg)
		{
			string instrumentName = Str(msg, "instrument");
			string timeframe = Str(msg, "timeframe");
			try
			{
				SubscribeToBars(session, instrumentName, timeframe);
			}
			catch (Exception ex)
			{
				SendError(session, "subscribe_bars failed for " + instrumentName + "/" + timeframe + ": " + ex.Message);
			}
		}

		internal void HandleSubscribeTicks(ClientSession session, Dictionary<string, object> msg)
		{
			// Ticks are derived from the finest subscribed bar timeframe - see
			// the file header. Ensure a 1m subscription exists; BarsSubscription
			// itself pushes "tick" messages alongside "bar" ones.
			string instrumentName = Str(msg, "instrument");
			try
			{
				SubscribeToBars(session, instrumentName, "1m");
			}
			catch (Exception ex)
			{
				SendError(session, "subscribe_ticks failed for " + instrumentName + ": " + ex.Message);
			}
		}

		/// Loops rather than a single GetOrAdd because the subscription
		/// RemoveClient just disposed (its last subscriber leaving) can still be
		/// the value GetOrAdd returns for a brief window before its barSubs
		/// entry is actually removed - AddSubscriber returns false in that case
		/// (see its docstring) so this retries with a fresh subscription instead
		/// of handing a disposed BarsRequest to a new subscriber.
		private void SubscribeToBars(ClientSession session, string instrumentName, string timeframe)
		{
			string key = instrumentName + "|" + timeframe;
			while (true)
			{
				var sub = barSubs.GetOrAdd(key, _ => new BarsSubscription(this, instrumentName, timeframe));
				if (sub.AddSubscriber(session)) return;
				((ICollection<KeyValuePair<string, BarsSubscription>>)barSubs)
					.Remove(new KeyValuePair<string, BarsSubscription>(key, sub));
			}
		}

		internal void HandleBackfillBars(ClientSession session, Dictionary<string, object> msg)
		{
			string reqId = Str(msg, "req_id");
			string instrumentName = Str(msg, "instrument");
			string timeframe = Str(msg, "timeframe");
			long sinceMs = (long)Dbl(msg, "since_ms", 0);
			long untilMs = (long)Dbl(msg, "until_ms", 0);

			try
			{
				Instrument instrument = Instrument.GetInstrument(instrumentName);
				if (instrument == null)
				{
					SendBackfillResult(session, reqId, new List<double[]>());
					return;
				}

				// NOT using the (Instrument, DateTime, DateTime) constructor here
				// - confirmed live 2026-10-12: NT8's real implementation rounds
				// the given range up to full LOCAL TRADING DAYS regardless of how
				// narrow a window was asked for, so even a 5-minute request for
				// 1m bars came back as ~1,380 bars (a full day) on one line,
				// large enough to exceed the Python side's StreamReader line
				// limit and desync the whole connection (LimitOverrunError).
				// The (Instrument, int) overload is used instead and the result is
				// trimmed back down to [sinceMs, untilMs] before sending. That int
				// is BARS back, not days back (NT8 docs: BarsRequest(Instrument,
				// int barsBack)) - it used to be fed a day count, so every
				// backfill returned only 1-4 bars (found 2026-11-11). Sized here
				// as span / bar length (+2 for the partial bars at either end),
				// capped so a response stays ~3.5 MB, well under the Python
				// side's 16 MB StreamReader limit. Session gaps (nights,
				// weekends) only make this over-request, and the trim removes
				// the excess.
				double spanMs = Math.Max(untilMs - sinceMs, 0);
				double barMs = TimeframeMs(timeframe);
				int barsBack = (int)Math.Min(Math.Ceiling(spanMs / barMs) + 2, 50000);
				barsBack = Math.Max(barsBack, 1);

				var request = new BarsRequest(instrument, barsBack)
				{
					BarsPeriod = MapTimeframe(timeframe),
				};
				request.Request((req, errorCode, errorMessage) =>
				{
					var rows = new List<double[]>();
					if (errorCode == ErrorCode.NoError)
					{
						Bars bars = req.Bars;
						for (int i = 0; i < bars.Count; i++)
						{
							long tsMs = DateToUnixMs(bars.GetTime(i));
							if (tsMs < sinceMs || tsMs > untilMs) continue;
							rows.Add(new[]
							{
								(double)tsMs,
								bars.GetOpen(i),
								bars.GetHigh(i),
								bars.GetLow(i),
								bars.GetClose(i),
								bars.GetVolume(i),
							});
						}
					}
					SendBackfillResult(session, reqId, rows);
				});
			}
			catch (Exception ex)
			{
				SendError(session, "backfill_bars failed: " + ex.Message);
				SendBackfillResult(session, reqId, new List<double[]>());
			}
		}

		private void SendBackfillResult(ClientSession session, string reqId, List<double[]> rows)
		{
			var sb = new StringBuilder();
			sb.Append("{\"type\":\"backfill_result\"");
			AppendStr(sb, "req_id", reqId);
			sb.Append(",\"bars\":[");
			for (int i = 0; i < rows.Count; i++)
			{
				if (i > 0) sb.Append(",");
				var r = rows[i];
				sb.Append("[").Append((long)r[0]).Append(",")
					.Append(r[1].ToString(CultureInfo.InvariantCulture)).Append(",")
					.Append(r[2].ToString(CultureInfo.InvariantCulture)).Append(",")
					.Append(r[3].ToString(CultureInfo.InvariantCulture)).Append(",")
					.Append(r[4].ToString(CultureInfo.InvariantCulture)).Append(",")
					.Append(r[5].ToString(CultureInfo.InvariantCulture)).Append("]");
			}
			sb.Append("]}");
			session.Send(sb.ToString());
		}

		internal static BarsPeriod MapTimeframe(string timeframe)
		{
			switch (timeframe)
			{
				case "1m": return new BarsPeriod { BarsPeriodType = BarsPeriodType.Minute, Value = 1 };
				case "5m": return new BarsPeriod { BarsPeriodType = BarsPeriodType.Minute, Value = 5 };
				case "15m": return new BarsPeriod { BarsPeriodType = BarsPeriodType.Minute, Value = 15 };
				case "30m": return new BarsPeriod { BarsPeriodType = BarsPeriodType.Minute, Value = 30 };
				case "1h": return new BarsPeriod { BarsPeriodType = BarsPeriodType.Minute, Value = 60 };
				case "4h": return new BarsPeriod { BarsPeriodType = BarsPeriodType.Minute, Value = 240 };
				case "1d": return new BarsPeriod { BarsPeriodType = BarsPeriodType.Day, Value = 1 };
				default: throw new ArgumentException("Unsupported timeframe: " + timeframe);
			}
		}

		// Bar length in ms for the same timeframe strings MapTimeframe accepts.
		internal static double TimeframeMs(string timeframe)
		{
			switch (timeframe)
			{
				case "1m": return 60000.0;
				case "5m": return 300000.0;
				case "15m": return 900000.0;
				case "30m": return 1800000.0;
				case "1h": return 3600000.0;
				case "4h": return 14400000.0;
				case "1d": return 86400000.0;
				default: throw new ArgumentException("Unsupported timeframe: " + timeframe);
			}
		}

		private static TimeInForce MapTif(string tif)
		{
			// NT8's real Cbi.TimeInForce enum has no Fok member (confirmed by
			// compiler: Ioc compiles, Fok does not) - FOK isn't a NT8 concept,
			// so it falls back to Gtc like any other unrecognized value rather
			// than guessing at a different member name.
			switch (tif)
			{
				case "DAY": return TimeInForce.Day;
				case "IOC": return TimeInForce.Ioc;
				default: return TimeInForce.Gtc;
			}
		}

		// -- account state polling -----------------------------------------------

		private void PushAccountStates(object state)
		{
			List<ClientSession> snapshot;
			lock (clientsLock) { snapshot = new List<ClientSession>(clients); }

			// Group by account so we only query NT8 once per distinct account
			// even if several clients/bots share it.
			foreach (var accountName in snapshot.Select(c => c.Account).Where(a => a != null).Distinct())
			{
				Account account = ResolveAccount(accountName);
				if (account == null) continue;

				string payload;
				try
				{
					payload = BuildAccountStatePayload(account);
				}
				catch (Exception ex)
				{
					Log("VulcanTraderBridge: account_state query failed for " + accountName + ": " + ex, LogLevel.Warning);
					continue;
				}

				foreach (var session in snapshot.Where(c => c.Account == accountName))
					session.Send(payload);
			}
		}

		private string BuildAccountStatePayload(Account account)
		{
			// Account.Get(AccountItem, Currency) and Account.Positions/Position.
			// Instrument/Quantity/AveragePrice/MarketPosition all confirmed
			// against a real NT8 compile+run.
			double cash = account.Get(AccountItem.CashValue, Currency.UsDollar);
			double netLiq = account.Get(AccountItem.NetLiquidation, Currency.UsDollar);
			double buyingPower = account.Get(AccountItem.BuyingPower, Currency.UsDollar);

			var sb = new StringBuilder();
			sb.Append("{\"type\":\"account_state\"");
			AppendNum(sb, "cash_value", cash);
			AppendNum(sb, "net_liquidation", netLiq);
			AppendNum(sb, "buying_power", buyingPower);
			sb.Append(",\"positions\":[");
			bool first = true;
			foreach (Position p in account.Positions)
			{
				if (p.MarketPosition == MarketPosition.Flat) continue;
				if (!first) sb.Append(",");
				first = false;
				sb.Append("{");
				string canonicalName = p.Instrument?.FullName;
				string reportedName = (canonicalName != null
					&& instrumentCanonicalToRequested.TryGetValue(canonicalName, out var requestedName))
					? requestedName
					: canonicalName;
				AppendStrRaw(sb, "instrument", reportedName, first: true);
				AppendNum(sb, "quantity", p.Quantity);
				AppendNum(sb, "avg_price", p.AveragePrice);
				AppendStr(sb, "market_position", p.MarketPosition.ToString());
				sb.Append("}");
			}
			sb.Append("]}");
			return sb.ToString();
		}

		// -- small JSON helpers (no external JSON library dependency) ------------

		internal static void AppendStr(StringBuilder sb, string key, string value)
		{
			sb.Append(",\"").Append(key).Append("\":").Append(value == null ? "null" : "\"" + Escape(value) + "\"");
		}

		internal static void AppendStrOrNull(StringBuilder sb, string key, string value) => AppendStr(sb, key, value);

		internal static void AppendStrRaw(StringBuilder sb, string key, string value, bool first)
		{
			if (!first) sb.Append(",");
			sb.Append("\"").Append(key).Append("\":").Append(value == null ? "null" : "\"" + Escape(value) + "\"");
		}

		internal static void AppendNum(StringBuilder sb, string key, double value)
		{
			sb.Append(",\"").Append(key).Append("\":").Append(value.ToString(CultureInfo.InvariantCulture));
		}

		internal static void AppendNumOrNull(StringBuilder sb, string key, double? value)
		{
			sb.Append(",\"").Append(key).Append("\":")
				.Append(value.HasValue ? value.Value.ToString(CultureInfo.InvariantCulture) : "null");
		}

		private static string Escape(string s) => s.Replace("\\", "\\\\").Replace("\"", "\\\"");

		internal static string Str(Dictionary<string, object> msg, string key) =>
			msg.TryGetValue(key, out var v) && v != null ? v.ToString() : null;

		internal static double Dbl(Dictionary<string, object> msg, string key, double fallback) =>
			msg.TryGetValue(key, out var v) && v != null ? Convert.ToDouble(v, CultureInfo.InvariantCulture) : fallback;

		internal static double? DblOrNull(Dictionary<string, object> msg, string key) =>
			msg.TryGetValue(key, out var v) && v != null ? (double?)Convert.ToDouble(v, CultureInfo.InvariantCulture) : null;

		internal static DateTime UnixMsToDate(long ms) => new DateTime(1970, 1, 1, 0, 0, 0, DateTimeKind.Utc).AddMilliseconds(ms);
		internal static long DateToUnixMs(DateTime dt) => (long)(dt.ToUniversalTime() - new DateTime(1970, 1, 1, 0, 0, 0, DateTimeKind.Utc)).TotalMilliseconds;

		// =====================================================================
		// Per-instrument/timeframe live bars, shared across every client that
		// subscribed to the same (instrument, timeframe) pair.
		// =====================================================================
		internal class BarsSubscription : IDisposable
		{
			private readonly VulcanTraderBridge bridge;
			private readonly string instrumentName;
			private readonly string timeframe;
			private readonly BarsRequest request;
			private readonly HashSet<ClientSession> subscribers = new HashSet<ClientSession>();
			private readonly object subLock = new object();
			private bool disposed;

			public BarsSubscription(VulcanTraderBridge bridge, string instrumentName, string timeframe)
			{
				this.bridge = bridge;
				this.instrumentName = instrumentName;
				this.timeframe = timeframe;

				Instrument instrument = Instrument.GetInstrument(instrumentName);
				if (instrument == null)
					throw new InvalidOperationException("Instrument '" + instrumentName + "' could not be resolved.");
				if (instrument.FullName != null)
					bridge.instrumentCanonicalToRequested[instrument.FullName] = instrumentName;

				// Confirmed against docs.ninjatrader.com/ninjascript/barsrequest:
				// there is no IsLiveUpdates flag - merely subscribing to .Update
				// is what makes a BarsRequest live (the docs: "BarsRequest can be
				// used to request Bars data and subscribe to real-time Bars data
				// events"). barsBack=5 gives OnBarsUpdate something to diff
				// against immediately; the real history for a strategy comes from
				// HandleBackfillBars, not this constructor's initial load.
				request = new BarsRequest(instrument, 5) { BarsPeriod = MapTimeframe(timeframe) };
				request.Update += OnBarsUpdate;
				request.Request((req, errorCode, errorMessage) =>
				{
					if (errorCode != ErrorCode.NoError)
						NinjaScript.Log(
							"VulcanTraderBridge: BarsRequest failed for " + instrumentName + "/" + timeframe + ": " + errorMessage,
							LogLevel.Warning);
				});
			}

			/// Returns false if this subscription was already disposed (its last
			/// subscriber left and RemoveClient tore it down) - the caller
			/// (SubscribeToBars) must then retry against a fresh subscription
			/// rather than add to one whose BarsRequest is gone.
			public bool AddSubscriber(ClientSession session)
			{
				lock (subLock)
				{
					if (disposed) return false;
					subscribers.Add(session);
					return true;
				}
			}

			/// Returns true if this was the last subscriber (the caller should
			/// then Dispose() this subscription and drop it from barSubs - see
			/// RemoveClient). Leaving an orphaned BarsSubscription's live
			/// BarsRequest+Update running forever once nobody is listening is a
			/// real, confirmed-live resource leak (2026-10-12): dozens of
			/// never-disposed subscriptions accumulated from short-lived test
			/// connections across one long session, and NT8's market-data
			/// subsystem subsequently degraded (new BarsRequest operations
			/// started failing/resetting the connection, while unrelated
			/// operations like Account.Get kept working fine - pointing at
			/// exhausted BarsRequest-specific resources, not a general outage).
			/// Sets `disposed` under the same lock so a concurrent AddSubscriber
			/// can never land on an instance about to be torn down.
			public bool RemoveSubscriber(ClientSession session)
			{
				lock (subLock)
				{
					subscribers.Remove(session);
					if (subscribers.Count == 0)
					{
						disposed = true;
						return true;
					}
					return false;
				}
			}

			private void OnBarsUpdate(object sender, BarsUpdateEventArgs e)
			{
				// Per the docs: a single tick can update more than one bar, so
				// the whole MinIndex..MaxIndex range must be processed, read off
				// e.BarsSeries (the event's own series reference) rather than
				// request.Bars. BarsSeries is a distinct type from Bars (confirmed
				// by a real NT8 compile: CS0029 narrowing-conversion error trying
				// to assign it to a Bars-typed variable) but exposes the same
				// GetTime/GetOpen/GetHigh/GetLow/GetClose/GetVolume/Count members
				// (confirmed via a NinjaTrader forum stack trace through
				// BarsSeries.GetClose) - `var` sidesteps needing its exact name.
				var bars = e.BarsSeries;
				if (bars == null || bars.Count == 0) return;

				List<ClientSession> targets;
				lock (subLock) { targets = new List<ClientSession>(subscribers); }
				if (targets.Count == 0) return;

				int minIdx = Math.Max(e.MinIndex, 0);
				int maxIdx = Math.Min(e.MaxIndex, bars.Count - 1);
				for (int idx = minIdx; idx <= maxIdx; idx++)
				{
					long tsMs = DateToUnixMs(bars.GetTime(idx));
					double open = bars.GetOpen(idx);
					double high = bars.GetHigh(idx);
					double low = bars.GetLow(idx);
					double close = bars.GetClose(idx);
					double volume = bars.GetVolume(idx);

					var barMsg = new StringBuilder();
					barMsg.Append("{\"type\":\"bar\"");
					AppendStr(barMsg, "instrument", instrumentName);
					AppendStr(barMsg, "timeframe", timeframe);
					barMsg.Append(",\"ts_ms\":").Append(tsMs);
					AppendNum(barMsg, "open", open);
					AppendNum(barMsg, "high", high);
					AppendNum(barMsg, "low", low);
					AppendNum(barMsg, "close", close);
					AppendNum(barMsg, "volume", volume);
					barMsg.Append("}");
					string barPayload = barMsg.ToString();

					var tickMsg = new StringBuilder();
					tickMsg.Append("{\"type\":\"tick\"");
					AppendStr(tickMsg, "instrument", instrumentName);
					tickMsg.Append(",\"ts_ms\":").Append(tsMs);
					AppendNum(tickMsg, "price", close);
					tickMsg.Append("}");
					string tickPayload = tickMsg.ToString();

					foreach (var session in targets)
					{
						session.Send(barPayload);
						session.Send(tickPayload);
					}
				}
			}

			public void Dispose()
			{
				try { request.Update -= OnBarsUpdate; } catch { }
				try { request.Dispose(); } catch { }
			}
		}

		// =====================================================================
		// One connected Python client (one VulcanTrader exchange instance, or a
		// shared registry connection for several bots in single_process_mode).
		// =====================================================================
		internal class ClientSession
		{
			private readonly VulcanTraderBridge bridge;
			private readonly TcpClient tcpClient;
			private readonly NetworkStream stream;
			private readonly StreamReader reader;
			private readonly object writeLock = new object();
			private volatile bool closed;

			public string Account { get; private set; }

			public ClientSession(VulcanTraderBridge bridge, TcpClient tcpClient)
			{
				this.bridge = bridge;
				this.tcpClient = tcpClient;
				stream = tcpClient.GetStream();
				reader = new StreamReader(stream, Encoding.UTF8);
			}

			public void ReadLoop()
			{
				try
				{
					string line;
					while (!closed && (line = reader.ReadLine()) != null)
					{
						if (line.Length == 0) continue;
						Dictionary<string, object> msg;
						try
						{
							msg = MiniJson.Parse(line);
						}
						catch (Exception ex)
						{
							bridge.SendError(this, "could not parse message: " + ex.Message);
							continue;
						}
						Dispatch(msg);
					}
				}
				catch (Exception)
				{
					// Connection dropped - expected on bot shutdown/reconnect.
				}
				finally
				{
					Close();
					bridge.RemoveClient(this);
				}
			}

			private void Dispatch(Dictionary<string, object> msg)
			{
				string type = Str(msg, "type");
				switch (type)
				{
					case "hello":
						Account = Str(msg, "account");
						bool exists = bridge.ResolveAccount(Account) != null;
						Send("{\"type\":\"hello_ack\",\"account_exists\":" + (exists ? "true" : "false") + "}");
						break;
					case "subscribe_bars":
						bridge.HandleSubscribeBars(this, msg);
						break;
					case "subscribe_ticks":
						bridge.HandleSubscribeTicks(this, msg);
						break;
					case "backfill_bars":
						bridge.HandleBackfillBars(this, msg);
						break;
					case "place_order":
						bridge.HandlePlaceOrder(this, msg);
						break;
					case "cancel_order":
						bridge.HandleCancelOrder(this, msg);
						break;
					default:
						bridge.SendError(this, "unknown message type: " + type);
						break;
				}
			}

			public void Send(string jsonLine)
			{
				if (closed) return;
				try
				{
					byte[] data = Encoding.UTF8.GetBytes(jsonLine + "\n");
					lock (writeLock)
					{
						stream.Write(data, 0, data.Length);
						stream.Flush();
					}
				}
				catch (Exception)
				{
					Close();
				}
			}

			public void Close()
			{
				if (closed) return;
				closed = true;
				try { reader?.Dispose(); } catch { }
				try { stream?.Dispose(); } catch { }
				try { tcpClient?.Close(); } catch { }
			}
		}

		// =====================================================================
		// Minimal hand-rolled JSON object parser - good enough for this
		// protocol's flat {"key": string|number|bool|null} messages. No nested
		// objects/arrays are ever sent FROM Python TO NT8 in this protocol, so
		// this intentionally doesn't need to handle them.
		// =====================================================================
		internal static class MiniJson
		{
			public static Dictionary<string, object> Parse(string json)
			{
				var result = new Dictionary<string, object>();
				int i = 0;
				SkipWs(json, ref i);
				Expect(json, ref i, '{');
				SkipWs(json, ref i);
				if (Peek(json, i) == '}') return result;
				while (true)
				{
					SkipWs(json, ref i);
					string key = ParseString(json, ref i);
					SkipWs(json, ref i);
					Expect(json, ref i, ':');
					SkipWs(json, ref i);
					object value = ParseValue(json, ref i);
					result[key] = value;
					SkipWs(json, ref i);
					char c = Peek(json, i);
					if (c == ',') { i++; continue; }
					if (c == '}') { i++; break; }
					throw new FormatException("Unexpected character at " + i + " in: " + json);
				}
				return result;
			}

			private static object ParseValue(string s, ref int i)
			{
				char c = Peek(s, i);
				if (c == '"') return ParseString(s, ref i);
				if (c == 't') { i += 4; return true; }
				if (c == 'f') { i += 5; return false; }
				if (c == 'n') { i += 4; return null; }
				int start = i;
				while (i < s.Length && (char.IsDigit(s[i]) || s[i] == '-' || s[i] == '+' || s[i] == '.' || s[i] == 'e' || s[i] == 'E'))
					i++;
				return double.Parse(s.Substring(start, i - start), CultureInfo.InvariantCulture);
			}

			private static string ParseString(string s, ref int i)
			{
				Expect(s, ref i, '"');
				var sb = new StringBuilder();
				while (Peek(s, i) != '"')
				{
					char c = s[i++];
					if (c == '\\')
					{
						char esc = s[i++];
						switch (esc)
						{
							case 'n': sb.Append('\n'); break;
							case 't': sb.Append('\t'); break;
							case 'r': sb.Append('\r'); break;
							case '"': sb.Append('"'); break;
							case '\\': sb.Append('\\'); break;
							case '/': sb.Append('/'); break;
							case 'u':
								sb.Append((char)Convert.ToInt32(s.Substring(i, 4), 16));
								i += 4;
								break;
							default: sb.Append(esc); break;
						}
					}
					else sb.Append(c);
				}
				i++; // closing quote
				return sb.ToString();
			}

			private static void SkipWs(string s, ref int i) { while (i < s.Length && char.IsWhiteSpace(s[i])) i++; }
			private static char Peek(string s, int i) => i < s.Length ? s[i] : '\0';
			private static void Expect(string s, ref int i, char c)
			{
				if (Peek(s, i) != c) throw new FormatException("Expected '" + c + "' at " + i + " in: " + s);
				i++;
			}
		}
	}
}
