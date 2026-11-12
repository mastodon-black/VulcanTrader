// ClientWebSocket exercise for the websocket.dll replacement (C# 5, .NET 4.x csc).
using System;
using System.Collections.Generic;
using System.IO;
using System.Net.WebSockets;
using System.Text;
using System.Threading;
using System.Threading.Tasks;

class WsTest
{
    static int fails = 0;

    static void Check(bool ok, string what)
    {
        Console.WriteLine((ok ? "PASS " : "FAIL ") + what);
        if (!ok) fails++;
    }

    static async Task<Tuple<WebSocketMessageType, byte[], WebSocketReceiveResult>> RecvMsg(ClientWebSocket ws, int chunk)
    {
        var ms = new MemoryStream();
        var buf = new byte[chunk];
        WebSocketReceiveResult r;
        do
        {
            r = await ws.ReceiveAsync(new ArraySegment<byte>(buf), CancellationToken.None);
            ms.Write(buf, 0, r.Count);
        } while (!r.EndOfMessage);
        return Tuple.Create(r.MessageType, ms.ToArray(), r);
    }

    static bool Same(byte[] a, byte[] b)
    {
        if (a.Length != b.Length) return false;
        for (int i = 0; i < a.Length; i++) if (a[i] != b[i]) return false;
        return true;
    }

    static async Task Run(string url)
    {
        var ws = new ClientWebSocket();
        await ws.ConnectAsync(new Uri(url), CancellationToken.None);
        Check(ws.State == WebSocketState.Open, "connect state=" + ws.State);

        var rnd = new Random(1);
        int[] sizes = { 0, 1, 125, 126, 127, 1000, 16383, 16384, 16385, 65535, 65536, 70000, 300000 };
        foreach (int n in sizes)
        {
            foreach (bool bin in new[] { false, true })
            {
                var data = new byte[n];
                if (bin) rnd.NextBytes(data); else for (int i = 0; i < n; i++) data[i] = (byte)('a' + i % 26);
                var copy = (byte[])data.Clone();
                await ws.SendAsync(new ArraySegment<byte>(data), bin ? WebSocketMessageType.Binary : WebSocketMessageType.Text, true, CancellationToken.None);
                Check(Same(data, copy), "send buffer unmodified n=" + n);
                var got = await RecvMsg(ws, n % 2 == 0 ? 4096 : 100);
                Check(got.Item1 == (bin ? WebSocketMessageType.Binary : WebSocketMessageType.Text) && Same(got.Item2, copy),
                      "echo " + (bin ? "bin " : "txt ") + n + " got " + got.Item1 + "/" + got.Item2.Length);
            }
        }

        // fragmented send (client side): 3 fragments -> server reassembles and echoes one message
        await ws.SendAsync(new ArraySegment<byte>(Encoding.UTF8.GetBytes("frag-A|")), WebSocketMessageType.Text, false, CancellationToken.None);
        await ws.SendAsync(new ArraySegment<byte>(Encoding.UTF8.GetBytes("frag-B|")), WebSocketMessageType.Text, false, CancellationToken.None);
        await ws.SendAsync(new ArraySegment<byte>(Encoding.UTF8.GetBytes("frag-C")), WebSocketMessageType.Text, true, CancellationToken.None);
        var f = await RecvMsg(ws, 4096);
        Check(Encoding.UTF8.GetString(f.Item2) == "frag-A|frag-B|frag-C", "client fragmented send -> " + Encoding.UTF8.GetString(f.Item2));

        // fragmented receive (server side)
        await ws.SendAsync(new ArraySegment<byte>(Encoding.UTF8.GetBytes("FRAGMENTED")), WebSocketMessageType.Text, true, CancellationToken.None);
        f = await RecvMsg(ws, 4096);
        Check(Encoding.UTF8.GetString(f.Item2) == "part1part2part3", "server fragmented reply -> " + Encoding.UTF8.GetString(f.Item2));

        // pings arrive while a receive is pending; server drops us if no PONG
        await ws.SendAsync(new ArraySegment<byte>(Encoding.UTF8.GetBytes("SLOW")), WebSocketMessageType.Text, true, CancellationToken.None);
        f = await RecvMsg(ws, 4096);
        Check(Encoding.UTF8.GetString(f.Item2) == "SLOW-OK", "survived pings during 3s wait -> " + Encoding.UTF8.GetString(f.Item2));

        // concurrent send while a receive is outstanding
        var recvTask = RecvMsg(ws, 4096);
        await ws.SendAsync(new ArraySegment<byte>(Encoding.UTF8.GetBytes("concurrent")), WebSocketMessageType.Text, true, CancellationToken.None);
        f = await recvTask;
        Check(Encoding.UTF8.GetString(f.Item2) == "concurrent", "send during pending receive");

        // client-initiated close
        await ws.CloseAsync(WebSocketCloseStatus.NormalClosure, "bye", CancellationToken.None);
        Check(ws.State == WebSocketState.Closed && ws.CloseStatus == WebSocketCloseStatus.NormalClosure,
              "client close state=" + ws.State + " status=" + ws.CloseStatus);

        // server-initiated close
        var ws2 = new ClientWebSocket();
        await ws2.ConnectAsync(new Uri(url), CancellationToken.None);
        await ws2.SendAsync(new ArraySegment<byte>(Encoding.UTF8.GetBytes("CLOSEME")), WebSocketMessageType.Text, true, CancellationToken.None);
        var buf = new byte[256];
        var r = await ws2.ReceiveAsync(new ArraySegment<byte>(buf), CancellationToken.None);
        Check(r.MessageType == WebSocketMessageType.Close && (int)r.CloseStatus == 4000 && r.CloseStatusDescription == "server bye",
              "server close type=" + r.MessageType + " status=" + (int?)r.CloseStatus + " reason=" + r.CloseStatusDescription);
        await ws2.CloseOutputAsync(WebSocketCloseStatus.NormalClosure, "", CancellationToken.None);
    }

    static int Main(string[] args)
    {
        try
        {
            if (!Task.Run(() => Run(args[0])).Wait(TimeSpan.FromSeconds(90)))
            {
                Console.WriteLine("FAIL timeout");
                return 3;
            }
        }
        catch (Exception e)
        {
            Console.WriteLine("FAIL exception: " + e);
            return 2;
        }
        Console.WriteLine(fails == 0 ? "ALL PASSED" : (fails + " FAILED"));
        return fails == 0 ? 0 : 1;
    }
}
