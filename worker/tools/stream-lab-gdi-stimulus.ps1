param(
    [ValidateRange(1, 60)]
    [int]$DurationSeconds = 20
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

$ddaStimulusForm = [System.Windows.Forms.Form]::new()
$ddaStimulusForm.Text = 'BubblePlay DDA isolation stimulus'
$ddaStimulusForm.StartPosition = 'Manual'
$ddaStimulusForm.Location = [System.Drawing.Point]::new(80, 80)
$ddaStimulusForm.ClientSize = [System.Drawing.Size]::new(480, 320)
$ddaStimulusForm.TopMost = $true
$ddaStimulusForm.FormBorderStyle = 'FixedToolWindow'

$ddaStimulusTimer = [System.Windows.Forms.Timer]::new()
$ddaStimulusTimer.Interval = 50
$script:ddaStimulusTicks = 0
$ddaStimulusLimit = $DurationSeconds * 20
$ddaStimulusTimer.Add_Tick({
    $script:ddaStimulusTicks++
    if (($script:ddaStimulusTicks % 2) -eq 0) {
        $ddaStimulusForm.BackColor = [System.Drawing.Color]::FromArgb(20, 110, 220)
    } else {
        $ddaStimulusForm.BackColor = [System.Drawing.Color]::FromArgb(240, 110, 30)
    }
    $ddaStimulusForm.Invalidate()
    if ($script:ddaStimulusTicks -ge $ddaStimulusLimit) {
        $ddaStimulusTimer.Stop()
        $ddaStimulusForm.Close()
    }
})
$ddaStimulusForm.Add_Shown({ $ddaStimulusTimer.Start() })
[System.Windows.Forms.Application]::Run($ddaStimulusForm)
$ddaStimulusTimer.Dispose()
$ddaStimulusForm.Dispose()
