--!strict

ui.stack { style = { clip = "circle" } }
ui.text { text = "clipped", style = { clip = true } }
ui.text_input { value = "", on_change = function() end, style = { clip = ui.transition { duration = 100, value = function() return 1 end } } }

return ui.panel {}
