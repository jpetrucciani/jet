# jet shell integration for PowerShell: prompt marks (OSC 133) and the
# working directory (OSC 9;9), wrapped around whatever prompt the profile set.
if ($null -eq $global:__JetPrompt) {
    $global:__JetPrompt = $function:prompt
    $global:__JetRan = $false
    function global:prompt {
        $ok = $?
        $code = $global:LASTEXITCODE
        $esc = [char]27
        $bel = [char]7
        $out = ''
        if ($global:__JetRan) {
            $status = if ($ok) { 0 } elseif ($code) { $code } else { 1 }
            $out += "$esc]133;D;$status$bel"
        }
        $global:__JetRan = $false
        $out += "$esc]133;A$bel"
        if ($PWD.Provider.Name -eq 'FileSystem') {
            $out += "$esc]9;9;$($PWD.ProviderPath)$bel"
        }
        $out += & $global:__JetPrompt
        $global:LASTEXITCODE = $code
        $out + "$esc]133;B$bel"
    }
    # A command starting (OSC 133;C) is only seen by PSReadLine: wrap Enter,
    # unless the profile bound it to something of its own.
    try {
        # -Bound, not -Chord: PSReadLine 2.0 (Windows PowerShell's) lacks it.
        $enter = Get-PSReadLineKeyHandler -Bound -ErrorAction Stop |
            Where-Object { $_.Key -eq 'Enter' } | Select-Object -First 1
        if ($enter.Function -eq 'AcceptLine') {
            Set-PSReadLineKeyHandler -Chord Enter -ScriptBlock {
                $line = $null
                $cursor = $null
                [Microsoft.PowerShell.PSConsoleReadLine]::GetBufferState([ref]$line, [ref]$cursor)
                [Microsoft.PowerShell.PSConsoleReadLine]::AcceptLine()
                if ($line.Trim()) {
                    $global:__JetRan = $true
                    [Console]::Write("$([char]27)]133;C$([char]7)")
                }
            }
        }
    } catch {}
}
