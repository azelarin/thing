--!strict

ui.timer { interval = "later", on_timeout = function() end }
ui.timer { interval = 10, on_timeout = "run" }
ui.timer { interval = 10 }
local timer = ui.timer { interval = 10, on_timeout = function() end }
timer:start("now")
timer.restart()
