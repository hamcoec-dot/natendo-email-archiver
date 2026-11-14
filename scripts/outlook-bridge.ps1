# Natendo Email Archiver: Outlook bridge.
# Talks to classic Outlook (Windows) through its COM automation interface.
# Output: one JSON object per line on stdout. Errors: {"type":"error",...}.
#
#   list                                   stores and mail folders with counts
#   export  -OutDir D [-Since T] [-Before T] [-Stores a|b] [-Folders x|y] [-Exclude x|y]
#                                          save matching mail items as .msg into D
#   delete  -List F                        delete the items listed in F
#                                          (lines: entryId<TAB>storeId); flagged items are kept
#
# Times (-Since / -Before) are Unix seconds. Items go to Deleted Items
# (Outlook's normal delete), so they can still be recovered there.

param(
    [Parameter(Position = 0, Mandatory = $true)][ValidateSet('list', 'export', 'delete')][string]$Mode,
    [string]$OutDir,
    [long]$Since = 0,
    [long]$Before = 0,
    [string]$Stores = '',
    [string]$Folders = '',
    [string]$Exclude = '',
    [string]$List
)

$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

function Emit($obj) { [Console]::Out.WriteLine(($obj | ConvertTo-Json -Compress -Depth 4)) }
function ToUnix([datetime]$d) { [long][Math]::Floor(($d.ToUniversalTime() - [datetime]'1970-01-01Z').TotalSeconds) }
function FromUnix([long]$t) { ([datetime]'1970-01-01Z').AddSeconds($t).ToLocalTime() }

try {
    $outlook = New-Object -ComObject Outlook.Application
    $ns = $outlook.GetNamespace('MAPI')
} catch {
    Emit @{ type = 'error'; message = "Could not start Outlook automation. Is classic Outlook installed and set up? $($_.Exception.Message)" }
    exit 2
}

$storeFilter = @($Stores -split '\|' | Where-Object { $_ })
$folderFilter = @($Folders -split '\|' | Where-Object { $_ })
$excludeFilter = @($Exclude -split '\|' | Where-Object { $_ })

function Walk($folder, [string]$path, $store) {
    # 0 = olMailItem folders only (skip calendars, contacts, tasks, notes)
    if ($folder.DefaultItemType -eq 0) {
        [pscustomobject]@{ Folder = $folder; Path = $path; Store = $store }
    }
    foreach ($sub in $folder.Folders) {
        Walk $sub ($path + '/' + $sub.Name) $store
    }
}

function MailFolders {
    foreach ($store in $ns.Stores) {
        $name = $store.DisplayName
        if ($storeFilter.Count -gt 0 -and -not ($storeFilter -contains $name)) { continue }
        try { $root = $store.GetRootFolder() } catch { continue }
        foreach ($top in $root.Folders) {
            foreach ($f in (Walk $top $top.Name $store)) {
                $lp = $f.Path.ToLowerInvariant()
                if ($folderFilter.Count -gt 0 -and -not ($folderFilter | Where-Object { $lp -eq $_.ToLowerInvariant() -or $lp.StartsWith($_.ToLowerInvariant() + '/') })) { continue }
                if ($excludeFilter | Where-Object { $lp.Contains($_.ToLowerInvariant()) }) { continue }
                $f
            }
        }
    }
}

switch ($Mode) {
    'list' {
        foreach ($f in (MailFolders)) {
            Emit @{ type = 'folder'; store = $f.Store.DisplayName; path = $f.Path; count = $f.Folder.Items.Count }
        }
    }
    'export' {
        if (-not $OutDir) { Emit @{ type = 'error'; message = '-OutDir is required' }; exit 2 }
        New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
        $n = 0
        foreach ($f in (MailFolders)) {
            $items = $f.Folder.Items
            $clauses = @()
            if ($Since -gt 0) { $clauses += "[ReceivedTime] >= '" + (FromUnix $Since).ToString('g') + "'" }
            if ($Before -gt 0) { $clauses += "[ReceivedTime] < '" + (FromUnix $Before).ToString('g') + "'" }
            if ($clauses.Count -gt 0) { $items = $items.Restrict(($clauses -join ' AND ')) }
            foreach ($it in $items) {
                try {
                    # 43 = MailItem; also keep reports/meeting mail (46, 53-57) as messages
                    if (@(43, 46, 53, 54, 55, 56, 57) -notcontains $it.Class) { continue }
                    $n++
                    $file = Join-Path $OutDir ("{0}.msg" -f $n)
                    $it.SaveAs($file, 9)  # olMSGUnicode
                    $received = try { ToUnix $it.ReceivedTime } catch { 0 }
                    $flagged = try { ($it.FlagStatus -eq 2) -or $it.IsMarkedAsTask } catch { $false }
                    $unread = try { [bool]$it.UnRead } catch { $false }
                    Emit @{ type = 'item'; file = $file; entryId = $it.EntryID; storeId = $f.Folder.StoreID; store = $f.Store.DisplayName; path = $f.Path; received = $received; flagged = $flagged; unread = $unread }
                } catch {
                    Emit @{ type = 'warning'; message = "$($f.Path): $($_.Exception.Message)" }
                } finally {
                    if ($it) { [void][System.Runtime.InteropServices.Marshal]::ReleaseComObject($it) }
                }
            }
        }
        Emit @{ type = 'done'; count = $n }
    }
    'delete' {
        $deleted = 0; $kept = 0
        foreach ($line in (Get-Content -LiteralPath $List -Encoding UTF8)) {
            $parts = $line -split "`t"
            if ($parts.Count -lt 2) { continue }
            try {
                $it = $ns.GetItemFromID($parts[0], $parts[1])
                $flagged = try { ($it.FlagStatus -eq 2) -or $it.IsMarkedAsTask } catch { $true }
                if ($flagged) { $kept++; continue }
                $it.Delete()
                $deleted++
            } catch {
                Emit @{ type = 'warning'; message = "delete: $($_.Exception.Message)" }
            }
        }
        Emit @{ type = 'done'; deleted = $deleted; kept = $kept }
    }
}
