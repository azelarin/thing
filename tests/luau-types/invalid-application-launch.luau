--!strict

local app = services.applications.entries:get()[1]
local _accepts: "file" | "files" = app.accepts
services.applications.launch(app)
services.applications.launch(app.id, { uris = '/tmp/a' })

return ui.panel {}
