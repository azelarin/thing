--!strict

ui.command(42, function() return nil end)
ui.command("toggle", "not a function")

return ui.panel {}
