<#
.SYNOPSIS
Measures where the sound stops between the player and RustDesk. Background: docs/troubleshooting.md

.DESCRIPTION
One self-contained file for Windows PowerShell 5.1 (powershell.exe). A development tool, not installed.
Changes no settings and saves no audio. Run it in the Windows session that plays the sound.

  Controlled PC    diagnose.ps1              about 25 s, does not interrupt the sound
                   diagnose.ps1 -Tone        also plays a faint sweep (-44 dB), so there is a known source
                   diagnose.ps1 -Exclusive   also records without Windows effects; interrupts other
                                             recorders of the cable (RustDesk) for about 14 s
  Controlling PC   diagnose.ps1 -Client      about 5 s: mute flags, audio events, what RustDesk plays

The report is printed and saved in %TEMP% (or -OutDir). FINDINGS are hints drawn from the numbers above
them: check those before concluding. The profile path, RustDesk IDs and IP addresses are masked; program
and device names are not.

.PARAMETER Cable
Recording end, e.g. 'Line 1 (Virtual Audio Cable)'. Default: derived from Ausieve's output.
.PARAMETER Output
Playback end Ausieve writes to. Default: from Ausieve's config.
.PARAMETER Exclude
Exe path of the excluded program (wildcards allowed), or 'none'. Default: from Ausieve's config.
#>
param(
    [switch]$Client,
    [switch]$Tone,
    [switch]$Exclusive,
    [string]$Cable = '',
    [string]$Output = '',
    [string]$Exclude = '',
    [string]$OutDir = $env:TEMP,
    [switch]$Player         # internal: the process that plays the tone
)
$ErrorActionPreference = 'Continue'
if ($Player) {
    # a 3 s sweep of changing level (so the waveforms can be aligned), looped, 70 s at most
    $ms = New-Object IO.MemoryStream; $w = New-Object IO.BinaryWriter($ms); $n = 144000
    $w.Write([Text.Encoding]::ASCII.GetBytes('RIFF')); $w.Write([int](36 + $n * 2)); $w.Write([Text.Encoding]::ASCII.GetBytes('WAVEfmt '))
    $w.Write([int]16); $w.Write([int16]1); $w.Write([int16]1); $w.Write([int]48000); $w.Write([int]96000); $w.Write([int16]2); $w.Write([int16]16)
    $w.Write([Text.Encoding]::ASCII.GetBytes('data')); $w.Write([int]($n * 2))
    $phase = 0.0
    for ($i = 0; $i -lt $n; $i++) {
        $x = $i / $n
        $phase += 6.283185307 * (300 + 1500 * $x * $x) / 48000
        $w.Write([int16](32767 * 0.0063 * (0.6 + 0.4 * [Math]::Sin(6.283185307 * 5 * $x)) * [Math]::Sin($phase)))
    }
    $w.Flush(); $ms.Position = 0
    $sound = New-Object Media.SoundPlayer($ms); $sound.Load(); $sound.PlayLooping()
    Start-Sleep -Seconds 70
    $sound.Stop(); exit
}
[Console]::OutputEncoding = [Text.Encoding]::UTF8

$code = @'
using System; using System.Text; using System.Threading; using System.Diagnostics;
using System.Collections.Generic; using System.Runtime.InteropServices;
namespace AusDiag {
[ComImport,Guid("BCDE0395-E52F-467C-8E3D-C4579291692E")] class En {}
[Guid("A95664D2-9614-4F35-A746-DE8DB63617E6"),InterfaceType(1),ComImport] interface IEn {
    void List(int flow,int state,out IColl c); [PreserveSig] int Def(int flow,int role,out IDev d); void Get([MarshalAs(UnmanagedType.LPWStr)] string id,out IDev d); }
[Guid("0BD7A1BE-7A1A-44DB-8397-CC5392387B5E"),InterfaceType(1),ComImport] interface IColl { void Count(out int n); void Item(int i,out IDev d); }
[Guid("D666063F-1587-4E43-81F1-B948E807363F"),InterfaceType(1),ComImport] interface IDev {
    void Act(ref Guid g,int c,IntPtr p,[MarshalAs(UnmanagedType.IUnknown)] out object o); void Props(int a,out IStore p); void Id([MarshalAs(UnmanagedType.LPWStr)] out string s); void State(out int s); }
[StructLayout(LayoutKind.Sequential)] struct Key { public Guid G; public int P; }
[StructLayout(LayoutKind.Sequential)] struct PV { public short Vt,a,b,c; public IntPtr V,W; }
[Guid("886D8EEB-8CF2-4446-8D02-CDBA1DBDCF99"),InterfaceType(1),ComImport] interface IStore { void a(); void b(); void Get(ref Key k,out PV v); }
[Guid("5CDF2C82-841E-4546-9722-0CF74078229A"),InterfaceType(1),ComImport] interface IVol {
    void a(); void b(); void c(); void d(); void e(); void f(); void Get(out float v); void h(); void i(); void j(); void k(); void l(); void Mute([MarshalAs(UnmanagedType.Bool)] out bool m); }
[Guid("C02216F6-8C67-4B5B-9D00-D008E73E0064"),InterfaceType(1),ComImport] interface IMet { void Peak(out float v); }
[Guid("87CE5498-68D6-44E5-9215-6DA47EF883D8"),InterfaceType(1),ComImport] interface ISv { void a(); void Get(out float v); void c(); void Mute([MarshalAs(UnmanagedType.Bool)] out bool m); }
[Guid("77AA99A0-1BD6-484F-8BC7-2C654C9A9B6F"),InterfaceType(1),ComImport] interface IMgr { void a(); void b(); void Sessions(out ISes s); }
[Guid("E2F5BB11-0570-40CA-ACDD-3AA01277DEE8"),InterfaceType(1),ComImport] interface ISes { void Count(out int n); void Get(int i,[MarshalAs(UnmanagedType.IUnknown)] out object o); }
[Guid("BFB7FF88-7239-4FC9-8FA2-07C950BE9C6D"),InterfaceType(1),ComImport] interface ICtl {
    void State(out int s); void b(); void c(); void d(); void e(); void f(); void g(); void h(); void i(); void j(); void k(); [PreserveSig] int Pid(out uint p); }
[Guid("1CB9AD4C-DBFA-4C32-B178-C2F568A703B2"),InterfaceType(1),ComImport] interface ICli {
    void Init(int mode,uint flags,long buffer,long period,IntPtr format,IntPtr session); void b(); void c(); void d(); void e(); void Mix(out IntPtr w); void g();
    void Start(); void Stop(); void Reset(); void Ev(IntPtr h); void Svc(ref Guid g,[MarshalAs(UnmanagedType.IUnknown)] out object o); }
[Guid("C8ADBD64-E71E-48A0-A4DE-185C395CD317"),InterfaceType(1),ComImport] interface ICap { void Buf(out IntPtr p,out int n,out int f,out long dp,out long q); void Rel(int n); void Next(out int n); }
[Guid("72A22D78-CDE4-431D-B8CC-843A71199B6D"),InterfaceType(1),ComImport] interface IOp { void Res(out int hr,[MarshalAs(UnmanagedType.IUnknown)] out object o); }
[Guid("41D949AB-9862-444A-80F6-C261334DA5EB"),InterfaceType(1),ComImport] interface IHan { void Done(IOp op); }
[Guid("94EA2B94-E9CC-49E0-C0FF-EE64CA8F5B90"),InterfaceType(1),ComImport] interface IAgile { }
[ComVisible(true)] public class Han : IHan, IAgile { public ManualResetEvent ev = new ManualResetEvent(false); void IHan.Done(IOp op) { ev.Set(); } }

public class Item { public string Flow="", Id="", Process="", Error=""; public bool IsSession, Active, Mute; public uint Pid; public float Volume, Peak; public int Rate; internal IMet Met; }
public class Endpoint { public string Flow="", Id="", Name=""; public int State; }
public class Cmp { public string Error="", Table=""; public float All, Wanted, Cable; public int Blocks; public double Corr, Gain, MinGain, Lag; }

class Rec {
    const int Len = P.Len;
    public float[] V = new float[Len];
    ICli c; ICap cap; AutoResetEvent ev = new AutoResetEvent(false);
    bool pcm, byTime; long t0; int pos; bool placed;
    public Rec(ICli client, int mode, uint flags, bool pcm16, bool timestamps) {
        c = client; pcm = pcm16; byTime = timestamps;
        IntPtr w = Marshal.AllocHGlobal(18); int al = pcm ? 4 : 8;
        Marshal.WriteInt16(w,0,(short)(pcm?1:3)); Marshal.WriteInt16(w,2,2); Marshal.WriteInt32(w,4,48000); Marshal.WriteInt32(w,8,48000*al);
        Marshal.WriteInt16(w,12,(short)al); Marshal.WriteInt16(w,14,(short)(al*4)); Marshal.WriteInt16(w,16,0);
        try {
            if (mode == 1) c.Init(1,0,1000000,100000,w,IntPtr.Zero);
            else { c.Init(0,flags|0x40000,20000000,0,w,IntPtr.Zero); c.Ev(ev.SafeWaitHandle.DangerousGetHandle()); }
        } finally { Marshal.FreeHGlobal(w); }
        Guid g = typeof(ICap).GUID; object o; c.Svc(ref g,out o); cap = (ICap)o;
    }
    public void Start(long origin) { t0 = origin; c.Start(); pos = (int)((P.Now()-t0)*48/10000); }
    public void Read() {
        int n; cap.Next(out n);
        while (n > 0) {
            IntPtr p; int f; long a,q; cap.Buf(out p,out n,out f,out a,out q);
            if (byTime) {
                // loopback timestamps are exact (a packet can be missing while nothing plays); allow for rounding
                int at = (int)Math.Max(-Len,Math.Min(Len,(q-t0)*48/10000));
                if (!placed || Math.Abs(at-pos) > 2) pos = at;
                placed = true;
            }
            // exclusive timestamps are not usable for placing samples, so the cable is joined in arrival order
            for (int i=0;i<n;i++,pos++) {
                if (pos<0 || pos>=Len || (f&2)!=0) continue;
                V[pos] = pcm ? Marshal.ReadInt16(p,i*4)/32768f : BitConverter.ToSingle(BitConverter.GetBytes(Marshal.ReadInt32(p,i*8)),0);
            }
            cap.Rel(n); cap.Next(out n);
        }
    }
    public float Max() { float m=0; foreach (float x in V) m=Math.Max(m,Math.Abs(x)); return m; }
    public void Close() { try { c.Stop(); } catch {} Marshal.ReleaseComObject(cap); Marshal.FinalReleaseComObject(c); }
}

public class P {
    [DllImport("Mmdevapi.dll",ExactSpelling=true,PreserveSig=false)]
    static extern void ActivateAudioInterfaceAsync([MarshalAs(UnmanagedType.LPWStr)] string path,ref Guid iid,IntPtr prop,IHan h,out IOp op);
    internal const int Len = 48000*15;
    internal static long Now() { return (long)(Stopwatch.GetTimestamp()*(1e7/Stopwatch.Frequency)); }
    static T A<T>(IDev d) { Guid g = typeof(T).GUID; object o; d.Act(ref g,23,IntPtr.Zero,out o); return (T)o; }
    static void Mta(ThreadStart work) { var t = new Thread(work); t.SetApartmentState(ApartmentState.MTA); t.Start(); t.Join(); }

    public static Item[] Snapshot() {
        var all = new List<Item>();
        Mta(delegate {
            try {
                var e = (IEn)(object)new En();
                for (int f=0;f<2;f++) {
                    string flow = f==0 ? "Render" : "Capture";
                    IDev def; string defId = "";
                    if (e.Def(f,0,out def)==0) def.Id(out defId);
                    IColl c; e.List(f,1,out c); int n; c.Count(out n);
                    for (int i=0;i<n;i++) {
                        IDev d; c.Item(i,out d); var it = new Item { Flow=flow }; d.Id(out it.Id); it.Active = it.Id==defId; all.Add(it);
                        try {
                            var v = A<IVol>(d); v.Get(out it.Volume); v.Mute(out it.Mute); it.Met = A<IMet>(d);
                            IntPtr w; A<ICli>(d).Mix(out w); it.Rate = Marshal.ReadInt32(w,4); Marshal.FreeCoTaskMem(w);
                            ISes l; A<IMgr>(d).Sessions(out l); int q; l.Count(out q);
                            for (int j=0;j<q;j++) {
                                object o; l.Get(j,out o); int st; var s = new Item { Flow=flow, Id=it.Id, IsSession=true };
                                ((ICtl)o).State(out st); s.Active = st==1; ((ICtl)o).Pid(out s.Pid);
                                ((ISv)o).Get(out s.Volume); ((ISv)o).Mute(out s.Mute); s.Met = (IMet)o;
                                try { s.Process = Process.GetProcessById((int)s.Pid).ProcessName; } catch { s.Process = "?"; }
                                all.Add(s);
                            }
                        } catch (Exception x) { it.Error = x.Message; }
                    }
                }
                for (int k=0;k<75;k++) {
                    foreach (var x in all) { if (x.Met==null) continue; try { float p; x.Met.Peak(out p); x.Peak=Math.Max(x.Peak,p); } catch {} }
                    Thread.Sleep(40);
                }
            } catch (Exception x) { all.Add(new Item { Error=x.ToString() }); }
        });
        return all.ToArray();
    }

    public static Endpoint[] Endpoints() {
        var all = new List<Endpoint>();
        Mta(delegate {
            var e = (IEn)(object)new En();
            for (int f=0;f<2;f++) {
                IColl c; int n;
                try { e.List(f,15,out c); c.Count(out n); } catch { continue; }
                for (int i=0;i<n;i++) {
                    try {
                        IDev d; c.Item(i,out d); var x = new Endpoint { Flow = f==0 ? "Render" : "Capture" };
                        d.Id(out x.Id); d.State(out x.State);
                        IStore s; d.Props(0,out s); PV v;
                        var k = new Key { G = new Guid("a45c254e-df1c-4efd-8020-67d146a850e0"), P = 14 };
                        s.Get(ref k,out v); if (v.Vt==31) x.Name = Marshal.PtrToStringUni(v.V);
                        all.Add(x);
                    } catch {}
                }
            }
        });
        return all.ToArray();
    }

    static ICli Loopback(int excludePid) {
        IntPtr ap = Marshal.AllocHGlobal(12), pv = Marshal.AllocHGlobal(24);
        Marshal.WriteInt32(ap,0,1); Marshal.WriteInt32(ap,4,excludePid); Marshal.WriteInt32(ap,8,1);
        for (int i=0;i<24;i++) Marshal.WriteByte(pv,i,0);
        Marshal.WriteInt16(pv,0,65); Marshal.WriteInt32(pv,8,12); Marshal.WriteIntPtr(pv,16,ap);
        var h = new Han(); IOp op; Guid iid = typeof(ICli).GUID;
        ActivateAudioInterfaceAsync("VAD\\Process_Loopback",ref iid,pv,h,out op);
        if (!h.ev.WaitOne(5000)) throw new Exception("process loopback activation timed out");
        int hr; object o; op.Res(out hr,out o);
        Marshal.FreeHGlobal(ap); Marshal.FreeHGlobal(pv);
        if (hr!=0) Marshal.ThrowExceptionForHR(hr);
        return (ICli)o;
    }

    public static Cmp Compare(string cableId, int excludePid, int selfPid, int exclusive) {
        var r = new Cmp();
        Mta(delegate {
            Rec all=null, want=null, cable=null;
            try {
                all = new Rec(Loopback(selfPid),0,0x20000,false,true);
                want = new Rec(Loopback(excludePid),0,0x20000,false,true);
                IDev d; ((IEn)(object)new En()).Get(cableId,out d);
                try { cable = new Rec(A<ICli>(d),exclusive,0x88000000,exclusive==1,false); }
                catch (Exception x) { r.Error = "cannot open the cable: 0x"+Marshal.GetHRForException(x).ToString("X8")+" "+x.Message; return; }
                long t0 = Now();
                all.Start(t0); want.Start(t0); cable.Start(t0);
                var sw = Stopwatch.StartNew();
                while (sw.ElapsedMilliseconds < 14000) { all.Read(); want.Read(); cable.Read(); Thread.Sleep(3); }
                r.All = all.Max(); r.Wanted = want.Max(); r.Cable = cable.Max();
                float[] recorded = cable.V;
                cable.Close(); cable = null; // let go of the cable before the slow analysis
                if (r.Wanted >= 0.0005 && r.Cable >= 0.00005) Analyse(want.V,recorded,r);
            } catch (Exception x) { r.Error = x.Message; }
            finally { foreach (var x in new Rec[]{all,want,cable}) if (x!=null) try { x.Close(); } catch {} }
        });
        return r;
    }

    static double Median(List<double> v) { var c = new List<double>(v); c.Sort(); return c[c.Count/2]; }

    static void Analyse(float[] R, float[] L, Cmp r) {
        const int N=9600, S=14400;
        var s = new StringBuilder("  t | refRMS | corr gain hf lagMs\r\n");
        var cs = new List<double>(); var gs = new List<double>(); var ls = new List<double>();
        for (int b=48000; b+N+S<48000*14; b+=48000) {
            double rr=0, rh=0;
            for (int i=b;i<b+N;i++) { rr+=R[i]*R[i]; double z=R[i]-R[i-1]; rh+=z*z; }
            if (rr < N*1e-6) continue; // RMS below 0.001: the cable carries 16 bits, rounding alone lowers corr and gain
            double top=0; int best=0;
            for (int k=-S;k<S;k++) {
                double xy=0, yy=0;
                for (int i=b;i<b+N;i+=5) { double y=L[i+k]; xy+=R[i]*y; yy+=y*y; }
                double score = Math.Abs(xy)/Math.Sqrt(yy+1e-12); if (score>top) { top=score; best=k; }
            }
            double XY=0, YY=0, YH=0;
            for (int i=b;i<b+N;i++) { double y=L[i+best]; XY+=R[i]*y; YY+=y*y; double z=y-L[i+best-1]; YH+=z*z; }
            double c = XY/Math.Sqrt(rr*YY+1e-20), g = Math.Sqrt(YY/rr);
            cs.Add(c); gs.Add(g); ls.Add(best/48.0);
            s.AppendLine(String.Format("  {0:F1} | {1:F5} | {2:F4} {3:F3} {4:F3} {5:F1}",b/48000.0,Math.Sqrt(rr/N),c,g,Math.Sqrt(YH/(rh+1e-20)),best/48.0));
        }
        r.Table = s.ToString(); r.Blocks = cs.Count;
        if (r.Blocks>0) { r.Corr=Median(cs); r.Gain=Median(gs); r.Lag=Median(ls); gs.Sort(); r.MinGain=gs[0]; }
    }
}
}
'@
if (-not ('AusDiag.P' -as [type])) { Add-Type -TypeDefinition $code }

$report = New-Object System.Collections.Generic.List[string]
$findings = New-Object System.Collections.Generic.List[string]
function Hide([string]$s) {
    $u = $env:USERPROFILE
    foreach ($form in $u.Replace('\', '\\'), $u, $u.Replace('\', '/')) { $s = $s -replace [regex]::Escape($form), '%USERPROFILE%' }
    return $s
}
function Add-Line([string]$s) {
    $s = Hide $s
    $report.Add($s)
    Write-Host $s
}
function Add-Finding([string]$s) { $findings.Add($s) }

$mm = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio'
$endpoints = @([AusDiag.P]::Endpoints() | ForEach-Object {
    $path = "$mm\$($_.Flow)\" + $_.Id.Substring($_.Id.LastIndexOf('.') + 1)
    $p = Get-ItemProperty "$path\Properties" -ErrorAction SilentlyContinue
    [pscustomobject]@{
        Flow = $_.Flow; Path = $path; Id = $_.Id; Name = $_.Name; State = $_.State
        AllowExclusive = $p.'{b3f8fa53-0004-438e-9003-51a46e139bfc},3'
    }
})
# the same rule as Ausieve: the exact name, else the only one containing it (active devices only)
function Find-Endpoint([string]$flow, [string]$name) {
    $active = @($endpoints | Where-Object { $_.Flow -eq $flow -and $_.State -eq 1 })
    $hit = @($active | Where-Object { $_.Name -eq $name })
    if ($hit.Count) { return $hit[0] }
    $hit = @($active | Where-Object { $_.Name -and $_.Name.IndexOf($name, [StringComparison]::OrdinalIgnoreCase) -ge 0 })
    if ($hit.Count -eq 1) { return $hit[0] }
}
function Get-EndpointName([string]$id) { ($endpoints | Where-Object { $_.Id -eq $id } | Select-Object -First 1).Name }

function Show-Snapshot($items) {
    foreach ($i in $items) {
        if ($i.IsSession) {
            $state = if ($i.Active) { 'ACTIVE' } else { 'idle' }
            Add-Line ('    {0} pid={1} {2} vol={3:F3} mute={4} peak={5:F4}' -f $i.Process, $i.Pid, $state, $i.Volume, $i.Mute, $i.Peak)
        } else {
            $def = if ($i.Active) { ' DEFAULT' } else { '' }
            Add-Line ('  {0} [{1}]{2} rate={3} vol={4:F3} mute={5} peak={6:F4} {7}' -f $i.Flow, (Get-EndpointName $i.Id), $def, $i.Rate, $i.Volume, $i.Mute, $i.Peak, $i.Error)
        }
    }
}

function Get-State($r) {
    if ($null -eq $r) { return 'skipped' }
    if ($r.Error) { return 'error' }
    if ([Math]::Max([double]$r.All, [double]$r.Wanted) -lt 0.0005) { return 'nothingPlaying' } # the tone is only in Wanted
    if ($r.Wanted -lt 0.0005) { return 'allExcluded' }
    if ($r.Cable -lt 0.00005) { return 'silent' }
    if ($r.Blocks -lt 3) { return 'tooQuiet' }
    if ($r.Corr -ge 0.998 -and $r.Gain -ge 0.97 -and $r.Gain -le 1.03) { return 'clean' }
    return 'damaged'
}

function Get-Verdict([string]$shared, [string]$excl, [bool]$recording) {
    $bad = @('silent', 'damaged')
    if ($shared -eq 'nothingPlaying') { return 'INCONCLUSIVE_NO_SOURCE: nothing was playing that process loopback can hear (it does not hear players in exclusive mode). Use -Tone or play something.' }
    if ($shared -eq 'allExcluded') { return 'INCONCLUSIVE_ALL_EXCLUDED: all that played was excluded. That alone is no fault: either only the excluded program made sound, or the player is in its process tree. -Tone gives a source outside it.' }
    if ($shared -eq 'tooQuiet') { return 'INCONCLUSIVE_TOO_QUIET: too little steady sound to compare. Use -Tone.' }
    if ($shared -eq 'clean') {
        if ($bad -contains $excl) { return 'INCONCLUSIVE_INCONSISTENT: clean in shared but not in exclusive recording (they are not simultaneous). Repeat with -Tone.' }
        if ($recording) { return 'CABLE_OK_AND_RECORDED: the cable is fine and RustDesk records it. Look at the controlling PC (-Client).' }
        return 'FAULT_AFTER_CABLE: the cable carries the correct sound but RustDesk does not record it. Look at RustDesk on this PC (it only records while a desktop connection is open and not muted).'
    }
    if ($excl -eq 'clean') { return 'FAULT_IN_WINDOWS_CAPTURE_PATH: correct without Windows effects, broken with them. Look at effects, volume, mute and privacy of the recording end.' }
    if ($bad -contains $excl) { return 'FAULT_BEFORE_CAPTURE: wrong even without Windows effects. Look at Ausieve (running, output, exclusion, tray error) and the cable.' }
    if ($shared -eq 'error') { return 'MEASUREMENT_FAILED: see the error in WAVEFORM. "cannot open the cable" 0x8889000A = held exclusively, 0x80070005 = denied; any other error is in the measurement itself: run again or measure another way.' }
    if ($excl -eq 'skipped') { return 'CABLE_WRONG: shared recording is wrong. If Ausieve runs, -Exclusive tells "before the recording end" from "Windows capture path".' }
    return "INCONCLUSIVE: shared=$shared, exclusive=$excl. Read WAVEFORM; -Tone gives a steady source."
}

function Get-Effects($e) {
    $dlls = @{}; $off = 'unset'
    $fx = Get-Item ($e.Path + '\FxProperties') -ErrorAction SilentlyContinue
    if ($fx) {
        foreach ($v in $fx.GetValueNames()) {
            if ($v -match '1da5d803-d492-4edd-8c23-e0c0ffee7f0e\},5$') { $off = $fx.GetValue($v) }
            foreach ($s in @($fx.GetValue($v))) {
                if ($s -is [string] -and $s -match '^\{[0-9A-Fa-f-]+\}$') {
                    $dll = (Get-ItemProperty -LiteralPath "HKLM:\SOFTWARE\Classes\CLSID\$s\InprocServer32" -ErrorAction SilentlyContinue).'(default)'
                    if ($dll) { $dlls[(Split-Path $dll -Leaf)] = 1 }
                }
            }
        }
    }
    [pscustomobject]@{ Dlls = ($dlls.Keys -join ','); Disabled = $off }
}

$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
Add-Line "Ausieve diagnosis $stamp  client=$Client  os=$([Environment]::OSVersion.VersionString)"

if ($Client) {
    Add-Line '## RUSTDESK MUTE FLAGS (newest peers first; not necessarily the one connected)'
    $n = 0
    Get-ChildItem "$env:APPDATA\RustDesk\config\peers" -ErrorAction SilentlyContinue | Sort-Object LastWriteTime -Descending | Select-Object -First 4 | ForEach-Object {
        $n++
        $m = Select-String -LiteralPath $_.FullName -Pattern '^disable_audio\s*=' | ForEach-Object { $_.Line }
        Add-Line ('  peer#{0} modified={1:MM-dd HH:mm:ss} {2}' -f $n, $_.LastWriteTime, $m)
        if ($n -eq 1 -and $m -match 'true') { Add-Finding 'CLIENT_MUTED: the most recently used peer has disable_audio = true. If that is the PC in question, unmute it in the RustDesk toolbar.' }
    }
    Add-Line '## RUSTDESK AUDIO EVENTS'
    Select-String -LiteralPath "$env:APPDATA\RustDesk\log\rustdesk_rCURRENT.log" -Pattern 'disable-audio|recved audio format|decoder loop exits|Audio playback .*(error|fail)' -ErrorAction SilentlyContinue |
        Select-Object -Last 14 | ForEach-Object { Add-Line ('  ' + ($_.Line -replace '\b\d{9,10}\b', '<id>' -replace '\d+\.\d+\.\d+\.\d+', 'x.x.x.x')) }
    Add-Line '## LOCAL METERS (3 s)'
    $snap = [AusDiag.P]::Snapshot()
    Show-Snapshot $snap
    $rd = @($snap | Where-Object { $_.IsSession -and $_.Flow -eq 'Render' -and $_.Active -and $_.Process -match 'rustdesk' })
    if ($rd.Count -eq 0) { Add-Finding 'CLIENT_NOT_PLAYING: RustDesk has no active playback stream here (not connected, muted, or no audio received).' }
    elseif (($rd | Measure-Object Peak -Maximum).Maximum -lt 0.001) { Add-Finding 'CLIENT_RECEIVES_SILENCE: RustDesk plays, but only silence. Meaningful only if the controlled PC was playing something during these 3 s.' }
    else { Add-Finding 'CLIENT_PLAYS_SOUND: RustDesk is playing sound here.' }
} else {
    # configuration
    $cfg = @(Get-Content "$env:APPDATA\Ausieve\config.toml" -Encoding UTF8 -ErrorAction SilentlyContinue)
    function Get-Setting([string]$key) {
        $m = $cfg | Where-Object { $_ -match "^\s*$key\s*=" } | Select-Object -First 1
        if (-not $m) { return '' }
        $v = ($m -split '=', 2)[1].Trim()
        if ($v.StartsWith('"')) { return $v.Trim('"').Replace('\\', '\') }
        return $v.Trim("'")
    }
    if (-not $Output) { $Output = Get-Setting 'output' }
    if (-not $Output) { $Output = 'Line 1 (Virtual Audio Cable)' }
    $outEnd = Find-Endpoint 'Render' $Output
    if ($outEnd) { $Output = $outEnd.Name }
    if (-not $Cable) { $Cable = $Output -replace '^(CABLE\S*) Input', '$1 Output' }
    $inEnd = Find-Endpoint 'Capture' $Cable
    if ($inEnd) { $Cable = $inEnd.Name }
    if (-not $Exclude) { $Exclude = Get-Setting 'selected' }
    Add-Line '## AUSIEVE CONFIG'
    if ($cfg.Count) { $cfg | ForEach-Object { Add-Line "  $_" } } else { Add-Line '  (no config file)' }

    Add-Line '## PROCESSES'
    $procs = @(Get-CimInstance Win32_Process)
    $procs | Where-Object { $_.Name -match '^(ausieve|capture|rustdesk)\.exe$' } |
        ForEach-Object { Add-Line ('  {0} parent={1} session={2} {3} {4}' -f $_.ProcessId, $_.ParentProcessId, $_.SessionId, $_.Name, $_.ExecutablePath) }
    $aus = @($procs | Where-Object { $_.Name -match '^(ausieve|capture)\.exe$' })
    $mySession = [Diagnostics.Process]::GetCurrentProcess().SessionId
    if ($aus | Where-Object { $_.Name -eq 'capture.exe' }) { Add-Line '  note: capture.exe takes output and exclusion from its arguments, not from the config: pass -Output and -Exclude.' }
    if ($aus.Count -eq 0) { Add-Finding 'AUSIEVE_NOT_RUNNING: nothing feeds the cable. Start Ausieve.' }
    elseif (-not ($aus | Where-Object { $_.SessionId -eq $mySession })) { Add-Finding 'AUSIEVE_OTHER_SESSION: Ausieve runs in another Windows session than this one.' }

    $targetPid = 0 # nothing excluded
    if ($Exclude -and $Exclude -ne 'none') {
        # as in Ausieve: * and ? stay inside one path segment; the root is the oldest one not started by the same program
        $pattern = [Environment]::ExpandEnvironmentVariables($Exclude).Replace('/', '\')
        $regex = '^' + [regex]::Escape($pattern).Replace('\*', '[^\\]*').Replace('\?', '[^\\]') + '$'
        $hit = @($procs | Where-Object { $_.ExecutablePath -and $_.ExecutablePath -match $regex })
        $root = $hit | Where-Object {
            $p = $_
            -not ($hit | Where-Object { $_.ProcessId -eq $p.ParentProcessId -and $_.CreationDate -le $p.CreationDate })
        } | Sort-Object CreationDate | Select-Object -First 1
        if ($root) {
            $targetPid = [int]$root.ProcessId
            Add-Line "## EXCLUDED TREE: $($root.Name) pid=$targetPid"
            $tree = @($targetPid)
            for ($d = 0; $d -lt 6; $d++) {
                $kids = @($procs | Where-Object { $tree -contains $_.ParentProcessId -and $tree -notcontains $_.ProcessId })
                foreach ($k in $kids) { $tree += $k.ProcessId; Add-Line ('  child {0} {1}' -f $k.ProcessId, $k.Name) }
            }
        } else { Add-Line "## EXCLUDED: $Exclude (not running, so nothing is excluded)" }
    }

    Add-Line '## RUSTDESK AUDIO INPUT (as saved for this user; no line: System sound, or only in the service profile)'
    Get-ChildItem "$env:APPDATA\RustDesk\config" -Filter 'RustDesk*.toml' -ErrorAction SilentlyContinue |
        Select-String -Pattern '^\s*audio-input\s*=' | ForEach-Object {
            Add-Line ('  ' + $_.Line)
            $chosen = ($_.Line -split '=', 2)[1].Trim().Trim([char[]]@([char]34, [char]39))
            if ($chosen -and -not ($endpoints | Where-Object { $_.Flow -eq 'Capture' -and $_.State -eq 1 -and $_.Name -eq $chosen })) {
                Add-Finding "RUSTDESK_INPUT_MISSING: no active recording device is called '$chosen'. RustDesk then records the default microphone."
            } elseif ($chosen -and $chosen -ne $Cable) {
                Add-Finding "RUSTDESK_OTHER_INPUT: it records '$chosen'; the cable's recording end is '$Cable'."
            }
        }

    Add-Line '## MICROPHONE PRIVACY'
    foreach ($h in 'HKLM:', 'HKCU:') {
        $key = "$h\SOFTWARE\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone"
        $a = (Get-ItemProperty $key -ErrorAction SilentlyContinue).Value
        $b = (Get-ItemProperty "$key\NonPackaged" -ErrorAction SilentlyContinue).Value
        Add-Line "  $h all=$a desktopApps=$b"
        if ($a -eq 'Deny' -or $b -eq 'Deny') { Add-Finding "MIC_PRIVACY_DENY: $h blocks recording, which includes virtual cables." }
    }

    Add-Line '## ENDPOINTS (state 1=active 2=disabled 4=absent 8=unplugged)'
    foreach ($e in $endpoints) {
        $isCable = ($e.Flow -eq 'Render' -and $e.Name -eq $Output) -or ($e.Flow -eq 'Capture' -and $e.Name -eq $Cable)
        if ($e.State -ne 1 -and -not $isCable) { continue }
        $fx = Get-Effects $e
        Add-Line ('  {0} [{1}] state={2} allowExclusive={3} disableSysFx={4} fx={5}' -f $e.Flow, $e.Name, $e.State, $e.AllowExclusive, $fx.Disabled, $fx.Dlls)
        if ($isCable -and $e.Flow -eq 'Capture' -and $fx.Dlls) { Add-Finding "CABLE_HAS_EFFECTS: $($fx.Dlls) is registered on the cable's recording end (disableSysFx=$($fx.Disabled)). Only a suspect until the waveform test agrees." }
        if ($isCable -and $e.Flow -eq 'Render' -and "$($e.AllowExclusive)" -eq '0') { Add-Finding 'CABLE_NO_EXCLUSIVE: exclusive mode is off on the playback end; Ausieve needs it.' }
    }
    $rdPath = $procs | Where-Object { $_.Name -eq 'rustdesk.exe' -and $_.ExecutablePath } | Select-Object -First 1 -ExpandProperty ExecutablePath
    if (-not $rdPath) {
        $svc = Get-CimInstance Win32_Service -Filter "Name='RustDesk'" -ErrorAction SilentlyContinue
        if ($svc.PathName -match '^"([^"]+)"') { $rdPath = $Matches[1] }
    }
    if ($rdPath -and (Test-Path -LiteralPath $rdPath)) {
        $ver = (Get-Item -LiteralPath $rdPath).VersionInfo.ProductVersion -replace '[+-].*$', ''
        Add-Line "## RUSTDESK VERSION $ver"
        $sameName = @($endpoints | Where-Object { $_.Flow -eq 'Render' -and $_.State -eq 1 -and $_.Name -eq $Cable }).Count
        $v = $ver -as [version]
        # before 1.5.3 audio_service.rs searched HOST.devices() (both directions) and took the first name match
        if ($sameName -and $v -and $v -lt [version]'1.5.3') {
            Add-Finding "RUSTDESK_PICKS_PLAYBACK_END: RustDesk $ver takes the first device called '$Cable', which is the playback end, and cannot record it. Rename the recording end and select it again, or update RustDesk to 1.5.3 or later."
        }
    }
    Add-Line '## AUDIO EFFECT DRIVERS'
    Get-PnpDevice -Class AudioProcessingObject -ErrorAction SilentlyContinue | ForEach-Object { Add-Line ('  {0} | {1}' -f $_.FriendlyName, $_.Status) }

    Add-Line '## LIVE (3 s of meters)'
    $snap = [AusDiag.P]::Snapshot()
    Show-Snapshot $snap
    if (-not $outEnd) { Add-Finding "OUTPUT_NOT_FOUND: no unique active playback device matches '$Output'. See ENDPOINTS (disabled?) or pass -Output." }
    else {
        $o = $snap | Where-Object { $_.Id -eq $outEnd.Id -and -not $_.IsSession }
        if ($o.Active) { Add-Finding 'CABLE_IS_DEFAULT_OUTPUT: programs play into the line Ausieve holds exclusively. Make the real speakers the default.' }
        if ($aus.Count -and -not ($snap | Where-Object { $_.Id -eq $outEnd.Id -and $_.IsSession -and $_.Active -and $_.Process -match '^(ausieve|capture)$' })) {
            Add-Finding 'AUSIEVE_NOT_WRITING: Ausieve runs but has no active stream on its output. Hover its tray icon for the error.'
        }
    }
    if (-not $inEnd) { Add-Finding "CABLE_NOT_FOUND: no unique active recording device matches '$Cable'. See ENDPOINTS (disabled or renamed?) or pass -Cable." }
    else {
        $c = $snap | Where-Object { $_.Id -eq $inEnd.Id -and -not $_.IsSession }
        if ($c.Mute -or $c.Volume -lt 0.01) { Add-Finding 'CABLE_MUTED: the recording end is muted or at zero volume.' }
        $recording = [bool]($snap | Where-Object { $_.Id -eq $inEnd.Id -and $_.IsSession -and $_.Active -and $_.Process -match 'rustdesk' })
        if (-not $recording) { Add-Finding 'RUSTDESK_NOT_RECORDING: no active RustDesk stream on the cable (no desktop connection, muted on the client, other input, or it failed to open).' }

        $modes = @(0); if ($Exclusive) { $modes += 1 }
        $res = @{}
        $tonePlayer = $null; $idle = $null
        try {
            if (-not $targetPid) {
                # "exclude nothing" = exclude a process that plays nothing. Not this one: its tree holds the tone player.
                $idle = Start-Process powershell.exe -NoNewWindow -PassThru -ArgumentList '-NoProfile -Command Start-Sleep -Seconds 120'
                $targetPid = $idle.Id
            }
            if ($Tone) {
                $tonePlayer = Start-Process powershell.exe -NoNewWindow -PassThru -ArgumentList ('-NoProfile -ExecutionPolicy RemoteSigned -File "{0}" -Player' -f $PSCommandPath)
                Start-Sleep -Seconds 5
            }
            foreach ($m in $modes) {
                $label = if ($m) { 'EXCLUSIVE (no Windows effects)' } else { 'SHARED (what RustDesk gets)' }
                Add-Line "## WAVEFORM $label - recording 14 s"
                $r = [AusDiag.P]::Compare($inEnd.Id, $targetPid, $PID, $m)
                $res[$m] = $r
                Add-Line ('  peak: allSystem={0:F5} afterExclusion={1:F5} cable={2:F5} {3}' -f $r.All, $r.Wanted, $r.Cable, $r.Error)
                if ($r.Table) { Add-Line $r.Table.TrimEnd() }
                if ($r.Blocks) {
                    Add-Line ('  median: corr={0:F4} gain={1:F3} lag={2:F1} ms; lowest gain={3:F3}' -f $r.Corr, $r.Gain, $r.Lag, $r.MinGain)
                    if ($r.MinGain -lt 0.9 -and (Get-State $r) -eq 'clean') { Add-Finding "UNEVEN ($label): the median is clean but some seconds are not. Noise suppression looks like this in quiet parts, and so does one short dropout: see the table." }
                }
            }
        } finally {
            foreach ($child in $tonePlayer, $idle) { if ($child -and -not $child.HasExited) { Stop-Process -Id $child.Id -Force -Confirm:$false } }
        }

        $s = Get-State $res[0]; $x = Get-State $res[1]
        Add-Finding "WAVEFORM: shared=$s exclusive=$x"
        Add-Finding (Get-Verdict $s $x $recording)

        if ($Exclusive) {
            Add-Line '## LIVE AFTER THE EXCLUSIVE TEST'
            $after = @([AusDiag.P]::Snapshot() | Where-Object { $_.Id -eq $inEnd.Id })
            Show-Snapshot $after
            if ($recording -and
                -not ($after | Where-Object { $_.IsSession -and $_.Active -and $_.Process -match 'rustdesk' })) {
                Add-Finding 'RUSTDESK_DID_NOT_RESUME: after the exclusive test, switch its audio input to System sound and back (older versions never reopen a failed stream).'
            }
        }
    }
}

Add-Line '## FINDINGS'
if ($findings.Count) { foreach ($f in $findings) { Add-Line "  $f" } } else { Add-Line '  none' }
$file = Join-Path $OutDir "ausieve-report-$stamp.txt"
$report | Out-File -FilePath $file -Encoding utf8
Write-Host (Hide "Report: $file")
