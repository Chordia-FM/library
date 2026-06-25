# This script has moved to the workspace root.
# Run from the Chordia root instead:
#
#   .\dev-reset.ps1
#
# The root version also cleans the Hub's PostgreSQL database, which this one did not.

Write-Host "Please run '.\dev-reset.ps1' from the Chordia workspace root instead." -ForegroundColor Yellow
Write-Host "That version also cleans the Hub database (server_directory, libraries)."
