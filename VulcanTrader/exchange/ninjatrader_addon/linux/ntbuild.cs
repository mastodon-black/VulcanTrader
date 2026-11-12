// ntbuild.cs - rebuild NinjaTrader.Custom.dll outside NinjaTrader with NT8's own bundled Roslyn.
// C# 5 (built by the .NET Framework csc). Usage:
//   ntbuild info  <oldDll>
//   ntbuild build <customDir> <ntBinDir> <oldDll> <outDir> [PRODUCTION]
// Sources: every *.cs under customDir except NinjaTrader.Vendor.cs (built separately into
// NinjaTrader.Vendor.dll). Main-assembly resources (Resource.resx strings) are copied
// unchanged from oldDll; culture satellite DLLs on disk are left alone.
using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Reflection;
using Microsoft.CodeAnalysis;
using Microsoft.CodeAnalysis.CSharp;
using Microsoft.CodeAnalysis.Emit;

class NtBuild
{
    static int Main(string[] a)
    {
        try { return a[0] == "info" ? Info(a[1]) : Build(a[1], a[2], a[3], a[4], a.Length > 5 && a[5] == "PRODUCTION"); }
        catch (Exception e) { Console.WriteLine("FATAL " + e); return 3; }
    }

    static int Info(string oldDll)
    {
        var asm = Assembly.ReflectionOnlyLoadFrom(oldDll);
        Console.WriteLine("name: " + asm.FullName);
        foreach (var r in asm.GetManifestResourceNames()) Console.WriteLine("resource: " + r);
        return 0;
    }

    static bool IsManaged(string path)
    {
        try { AssemblyName.GetAssemblyName(path); return true; } catch { return false; }
    }

    static int Build(string customDir, string ntBin, string oldDll, string outDir, bool production)
    {
        string fw = Path.GetDirectoryName(typeof(object).Assembly.Location);
        var sources = Directory.GetFiles(customDir, "*.cs", SearchOption.AllDirectories)
            .Where(f => !Path.GetFileName(f).Equals("NinjaTrader.Vendor.cs", StringComparison.OrdinalIgnoreCase))
            .OrderBy(f => f).ToList();
        var symbols = production ? new[] { "TRACE", "PRODUCTION" } : new[] { "TRACE" };
        var parse = new CSharpParseOptions(LanguageVersion.CSharp13, DocumentationMode.Parse, SourceCodeKind.Regular, symbols);
        var trees = sources.Select(f => CSharpSyntaxTree.ParseText(
            Microsoft.CodeAnalysis.Text.SourceText.From(File.ReadAllText(f), System.Text.Encoding.UTF8), parse, f)).ToList();

        var refPaths = new List<string>();
        string[] fwNames = { "mscorlib", "System", "System.Core", "System.Data", "System.Drawing", "System.Xml", "System.Xml.Linq",
            "System.Xaml", "System.Windows.Forms", "System.ComponentModel.DataAnnotations", "System.Net.Http", "System.Web.Extensions",
            "System.Runtime.Serialization", "Microsoft.CSharp", "System.Configuration", "System.Numerics", "System.ServiceModel",
            "System.Runtime", "netstandard", "System.Threading.Tasks", "System.IO.Compression", "System.Security", "System.Web",
            "System.Speech", "System.Transactions", "System.Net", "System.Data.DataSetExtensions" };
        foreach (var n in fwNames) { var p = Path.Combine(fw, n + ".dll"); if (File.Exists(p)) refPaths.Add(p); }
        foreach (var n in new[] { "WindowsBase", "PresentationCore", "PresentationFramework", "UIAutomationProvider", "UIAutomationTypes",
                                  "PresentationFramework.Aero", "ReachFramework", "System.Printing", "WindowsFormsIntegration" })
        {
            var p = Path.Combine(Path.Combine(fw, "WPF"), n + ".dll"); if (File.Exists(p)) refPaths.Add(p);
        }
        foreach (var p in Directory.GetFiles(ntBin, "*.dll", SearchOption.AllDirectories))
        {
            string fn = Path.GetFileName(p);
            if (fn.StartsWith("NinjaTrader.Custom", StringComparison.OrdinalIgnoreCase)) continue;
            // NinjaTrader.Client (ATI client) duplicates NinjaTrader.Core types; the csproj never references it.
            if (fn.Equals("NinjaTrader.Client.dll", StringComparison.OrdinalIgnoreCase)) continue;
            if (refPaths.Any(r => Path.GetFileName(r).Equals(fn, StringComparison.OrdinalIgnoreCase))) continue;
            if (IsManaged(p)) refPaths.Add(p);
        }
        var vendor = Path.Combine(customDir, "NinjaTrader.Vendor.dll");
        if (File.Exists(vendor)) refPaths.Add(vendor);
        var refs = refPaths.Select(p => (MetadataReference)MetadataReference.CreateFromFile(p)).ToList();
        Console.WriteLine("sources: " + trees.Count + "  references: " + refs.Count + "  PRODUCTION=" + production);

        var options = new CSharpCompilationOptions(OutputKind.DynamicallyLinkedLibrary,
            optimizationLevel: OptimizationLevel.Release, platform: Platform.X64, allowUnsafe: true,
            assemblyIdentityComparer: DesktopAssemblyIdentityComparer.Default)
            .WithSpecificDiagnosticOptions(new Dictionary<string, ReportDiagnostic> { { "CS1591", ReportDiagnostic.Suppress } });
        var comp = CSharpCompilation.Create("NinjaTrader.Custom", trees, refs, options);

        var oldAsm = Assembly.ReflectionOnlyLoadFrom(oldDll);
        var resources = oldAsm.GetManifestResourceNames().Select(n =>
        {
            string name = n;
            return new ResourceDescription(name, () => oldAsm.GetManifestResourceStream(name), true);
        }).ToList();

        Directory.CreateDirectory(outDir);
        string dll = Path.Combine(outDir, "NinjaTrader.Custom.dll");
        string pdb = Path.Combine(outDir, "NinjaTrader.Custom.pdb");
        string xml = Path.Combine(outDir, "NinjaTrader.Custom.xml");
        EmitResult res;
        using (var pe = File.Create(dll)) using (var pd = File.Create(pdb)) using (var xd = File.Create(xml))
            res = comp.Emit(pe, pd, xd, manifestResources: resources);

        var errors = res.Diagnostics.Where(d => d.Severity == DiagnosticSeverity.Error).ToList();
        foreach (var d in errors.Take(40)) Console.WriteLine(d.ToString());
        int warnings = res.Diagnostics.Count(d => d.Severity == DiagnosticSeverity.Warning);
        Console.WriteLine(res.Success ? ("BUILD OK  errors=0 warnings=" + warnings + "  resources=" + resources.Count)
                                      : ("BUILD FAILED  errors=" + errors.Count));
        return res.Success ? 0 : 1;
    }
}
