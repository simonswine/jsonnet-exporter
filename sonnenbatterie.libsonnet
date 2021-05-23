// vim: ft=jsonnet.dev

// this is an example of a sonnenbatterie /v1/status endpoint
// sonnenbatterie eco 8.03 9010 ND
local input = { body: {
  Apparent_output: 79,
  BackupBuffer: '0',
  BatteryCharging: false,
  BatteryDischarging: false,
  Consumption_Avg: 497,
  Consumption_W: 505,
  Fac: 49.98500061035156,
  FlowConsumptionBattery: false,
  FlowConsumptionGrid: true,
  FlowConsumptionProduction: true,
  FlowGridBattery: false,
  FlowProductionBattery: false,
  FlowProductionGrid: false,
  GridFeedIn_W: -39,
  IsSystemInstalled: 1,
  OperatingMode: '2',
  Pac_total_W: -6,
  Production_W: 451,
  RSOC: 98,
  RemainingCapacity_W: 7302,
  Sac1: 25,
  Sac2: 26,
  Sac3: 28,
  SystemStatus: 'OnGrid',
  Timestamp: '2021-05-15 19:49:47',
  USOC: 98,
  Uac: 237,
  Ubat: 50,
  dischargeNotAllowed: false,
  generator_autostart: false,
} };

local process(input) = {
  local this = self,
  solar_battery_charge_percent: {
    type: 'gauge',
    help: 'Percentage of battery charge.',
    series: [{
      value: input.body.RSOC,
    }],
  },
  solar_battery_grid_voltage: {
    type: 'gauge',
    help: 'Grid voltage in V',
    series: [{
      value: input.body.Uac,
    }],
  },
  solar_battery_voltage: {
    type: 'gauge',
    help: 'Battery voltage in V',
    series: [{
      value: input.body.Ubat,
    }],
  },
  solar_battery_grid_frequency_hz: {
    type: 'gauge',
    help: 'Grid frequency in Hz',
    series: [{
      value: input.body.Fac,
    }],
  },

};

{
  process(input):: process(input),
  test01: $.process(input),
}

// Uncomment to debug with example data
//process(input)



