--!strict

local app = services.applications.entries:get()[1]
local _directory: string = app.working_directory
local _first: number = app.command[1]
services.applications.entries:set({})

return ui.panel {}
